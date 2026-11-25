# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller-side font metadata and shape-aware translation.

This module is the Python counterpart of the ``ozma-fonts`` Rust crate. It
captures lossless metadata from TTF/OTF/TTC faces, derives the same shape and
coverage fields, and scores candidate fonts against a :class:`FontIntent` using
the same weights, hard filters, and deterministic ordering as the Rust
implementation.

Translation is an export decision. :func:`plan_export` always returns a
:class:`FontTranslationReport`; the original font metadata is never discarded.
Reports serialise to the ``schemas/display/font-v1.schema.json`` shape.
"""

from __future__ import annotations

import hashlib
import math
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

# ---------------------------------------------------------------------------
# Scoring weights (kept in sync with ozma-fonts/src/matching.rs)
# ---------------------------------------------------------------------------

WEIGHT_PANOSE = 0.22
WEIGHT_METRICS = 0.30
WEIGHT_COVERAGE = 0.26
WEIGHT_FEATURES = 0.10
WEIGHT_ROLE = 0.07
WEIGHT_FORMAT = 0.05

SHA256_HEX_LEN = 64

_BITMAP_FORMATS = frozenset({"bdf", "pcf", "bitmap", "lvgl_c"})

_FORMAT_ALIASES = {
    "truetype": "ttf",
    "true": "ttf",
    "opentype": "otf",
    "cff": "otf",
    "woff": "woff",
    "woff2": "woff2",
    "ttc": "ttc",
    "collection": "ttc",
    "lvgl": "lvgl_c",
    "lvgl-c": "lvgl_c",
    "lvgl_c": "lvgl_c",
    "bdf": "bdf",
    "pcf": "pcf",
    "bitmap": "bitmap",
    "raster": "bitmap",
    "atlas": "bitmap",
    "pbm": "bitmap",
}

_FEATURE_ALIASES = {
    "tnum": "tabular_nums",
    "pnum": "proportional_nums",
    "lnum": "lining_nums",
    "onum": "oldstyle_nums",
    "zero": "slashed_zero",
    "liga": "ligatures",
    "dlig": "discretionary_ligatures",
    "clig": "contextual_ligatures",
    "calt": "contextual_alternates",
    "frac": "fractions",
    "sups": "superscript",
    "subs": "subscript",
    "smcp": "small_caps",
    "c2sc": "caps_to_small_caps",
    "kern": "kerning",
    "locl": "localized_forms",
    "salt": "stylistic_alternates",
    "case": "case_sensitive",
    "ordn": "ordinals",
    "numr": "numerator",
    "dnom": "denominator",
}

_WIDTH_NAMES = {
    1: "ultra-condensed",
    2: "extra-condensed",
    3: "condensed",
    4: "semi-condensed",
    5: "normal",
    6: "semi-expanded",
    7: "expanded",
    8: "extra-expanded",
    9: "ultra-expanded",
}

_WIDTH_NUMBERS = {
    "ultra-condensed": 1.0,
    "extra-condensed": 2.0,
    "condensed": 3.0,
    "semi-condensed": 4.0,
    "normal": 5.0,
    "regular": 5.0,
    "semi-expanded": 6.0,
    "expanded": 7.0,
    "extra-expanded": 8.0,
    "ultra-expanded": 9.0,
}

# Unicode ranges used to derive script coverage, matching metadata.rs.
_SCRIPT_RANGES: tuple[tuple[str, tuple[tuple[int, int], ...]], ...] = (
    (
        "latin",
        (
            (0x0041, 0x005A),
            (0x0061, 0x007A),
            (0x00C0, 0x00FF),
            (0x2C60, 0x2C7F),
            (0xA720, 0xA7FF),
        ),
    ),
    (
        "latin-ext",
        ((0x0100, 0x024F), (0x1E00, 0x1EFF), (0x2C60, 0x2C7F), (0xA720, 0xA7FF)),
    ),
    ("greek", ((0x0370, 0x03FF), (0x1F00, 0x1FFF))),
    ("cyrillic", ((0x0400, 0x052F), (0x2DE0, 0x2DFF), (0xA640, 0xA69F))),
    ("hebrew", ((0x0590, 0x05FF), (0xFB1D, 0xFB4F))),
    (
        "arabic",
        (
            (0x0600, 0x06FF),
            (0x0750, 0x077F),
            (0x08A0, 0x08FF),
            (0xFB50, 0xFDFF),
            (0xFE70, 0xFEFF),
        ),
    ),
    ("devanagari", ((0x0900, 0x097F), (0xA8E0, 0xA8FF))),
    ("bengali", ((0x0980, 0x09FF),)),
    ("gurmukhi", ((0x0A00, 0x0A7F),)),
    ("gujarati", ((0x0A80, 0x0AFF),)),
    ("oriya", ((0x0B00, 0x0B7F),)),
    ("tamil", ((0x0B80, 0x0BFF),)),
    ("telugu", ((0x0C00, 0x0C7F),)),
    ("kannada", ((0x0C80, 0x0CFF),)),
    ("malayalam", ((0x0D00, 0x0D7F),)),
    ("sinhala", ((0x0D80, 0x0DFF),)),
    ("thai", ((0x0E00, 0x0E7F),)),
    ("lao", ((0x0E80, 0x0EFF),)),
    ("tibetan", ((0x0F00, 0x0FFF),)),
    ("myanmar", ((0x1000, 0x109F), (0xA9E0, 0xA9FF), (0xAA60, 0xAA7F))),
    ("georgian", ((0x10A0, 0x10FF), (0x1C90, 0x1CBF), (0x2D00, 0x2D2F))),
    (
        "hangul",
        ((0x1100, 0x11FF), (0x3130, 0x318F), (0xA960, 0xA97F), (0xAC00, 0xD7AF)),
    ),
    ("ethiopic", ((0x1200, 0x137F), (0x1380, 0x139F), (0x2D80, 0x2DDF))),
    ("cherokee", ((0x13A0, 0x13FF), (0xAB70, 0xABBF))),
    ("canadian-aboriginal", ((0x1400, 0x167F), (0x18B0, 0x18FF))),
    ("khmer", ((0x1780, 0x17FF), (0x19E0, 0x19FF))),
    ("mongolian", ((0x1800, 0x18AF),)),
    ("hiragana", ((0x3040, 0x309F),)),
    ("katakana", ((0x30A0, 0x30FF), (0x31F0, 0x31FF), (0xFF66, 0xFF9F))),
    (
        "cjk",
        (
            (0x2E80, 0x2FFF),
            (0x3400, 0x4DBF),
            (0x4E00, 0x9FFF),
            (0xF900, 0xFAFF),
            (0x20000, 0x2FA1F),
        ),
    ),
    ("yi", ((0xA000, 0xA4CF),)),
    ("emoji", ((0x1F000, 0x1FAFF), (0x2600, 0x27BF))),
    (
        "symbols",
        ((0x2000, 0x25FF), (0x2B00, 0x2BFF), (0x1F000, 0x1FAFF)),
    ),
    (
        "private-use",
        ((0xE000, 0xF8FF), (0xF0000, 0xFFFFD), (0x100000, 0x10FFFD)),
    ),
)


class FontError(ValueError):
    """A font could not be read or parsed."""


class FontUnsupportedError(FontError):
    """The container is understood but not supported for metadata capture."""


# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class FontNameRecord:
    """A decoded ``name`` table record, retained verbatim."""

    name_id: int
    platform: str
    encoding_id: int
    language_id: int
    value: str

    def to_dict(self) -> dict[str, Any]:
        return {
            "name_id": self.name_id,
            "platform": self.platform,
            "encoding_id": self.encoding_id,
            "language_id": self.language_id,
            "value": self.value,
        }


@dataclass(frozen=True)
class VariableAxis:
    """One ``fvar`` axis with its full range."""

    tag: str
    name: str | None
    min: float
    default: float
    max: float
    hidden: bool

    def to_dict(self) -> dict[str, Any]:
        return {
            "tag": self.tag,
            "name": self.name,
            "min": self.min,
            "default": self.default,
            "max": self.max,
            "hidden": self.hidden,
        }


@dataclass(frozen=True)
class EmbeddingPermissions:
    """Decoded OS/2 ``fsType`` embedding bits."""

    fs_type: int | None = None
    embedding_allowed: bool = False
    subset_allowed: bool = False
    bitmap_only: bool = False
    preview_print_only: bool = False
    no_subsetting: bool = False
    restricted: bool = False

    @classmethod
    def from_fs_type(cls, fs_type: int | None) -> "EmbeddingPermissions":
        if fs_type is None:
            return cls()
        restricted = bool(fs_type & 0x0002)
        preview_print_only = bool(fs_type & 0x0004)
        no_subsetting = bool(fs_type & 0x0100)
        bitmap_only = bool(fs_type & 0x0200)
        embedding_allowed = not (restricted or preview_print_only or bitmap_only)
        return cls(
            fs_type=int(fs_type),
            embedding_allowed=embedding_allowed,
            subset_allowed=embedding_allowed and not no_subsetting,
            bitmap_only=bitmap_only,
            preview_print_only=preview_print_only,
            no_subsetting=no_subsetting,
            restricted=restricted,
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "fs_type": self.fs_type,
            "embedding_allowed": self.embedding_allowed,
            "subset_allowed": self.subset_allowed,
            "bitmap_only": self.bitmap_only,
            "preview_print_only": self.preview_print_only,
            "no_subsetting": self.no_subsetting,
            "restricted": self.restricted,
        }


@dataclass(frozen=True)
class Panose:
    """The ten PANOSE bytes."""

    values: tuple[int, ...]

    @classmethod
    def parse(cls, value: str | None) -> "Panose | None":
        if value is None:
            return None
        parts = [part for part in re.split(r"[\s\-:,;]+", value.strip()) if part]
        if len(parts) != 10:
            return None
        bytes_out: list[int] = []
        for part in parts:
            if part.lower().startswith("0x"):
                try:
                    number = int(part[2:], 16)
                except ValueError:
                    return None
            else:
                try:
                    number = int(part, 10)
                except ValueError:
                    return None
            if number < 0 or number > 255:
                return None
            bytes_out.append(number)
        return cls(tuple(bytes_out))

    def __str__(self) -> str:
        return " ".join(str(value) for value in self.values)

    @property
    def family_kind(self) -> int:
        return self.values[0]

    @property
    def serif_style(self) -> int:
        return self.values[1]

    @property
    def weight(self) -> int:
        return self.values[2]

    @property
    def proportion(self) -> int:
        return self.values[3]

    @property
    def stroke_variation(self) -> int:
        return self.values[5]

    @property
    def is_empty(self) -> bool:
        return all(value == 0 for value in self.values)


@dataclass
class FontMetadata:
    """Lossless metadata for one font face.

    The first group mirrors the ``FontAsset`` contract; the remaining fields are
    derived from the face and retained alongside the original values.
    """

    id: str = ""
    family: str = ""
    style: str = ""
    version: str | None = None
    format: str = ""
    sha256: str = ""
    source: str = "imported"
    license: str | None = None
    embedding_allowed: bool = False
    subset_allowed: bool = False
    units_per_em: int = 0
    ascent: int = 0
    descent: int = 0
    x_height: int | None = None
    cap_height: int | None = None
    panose: str | None = None
    os2_family_class: int | None = None
    variable_axes: dict[str, float] = field(default_factory=dict)
    glyph_coverage: list[str] = field(default_factory=list)
    scripts: list[str] = field(default_factory=list)
    features: list[str] = field(default_factory=list)
    private: bool = True

    # Extended asset fields retained for auditing and export.
    size_bytes: int = 0
    license_url: str | None = None
    copyright: str | None = None
    subset: bool = False
    fs_type: int | None = None
    bitmap_only: bool = False

    # Additional derived metadata (same names as the Rust model).
    classification: str = ""
    weight: int = 400
    width: int = 5
    width_name: str = "normal"
    slant: str = "upright"
    is_variable: bool = False
    is_monospaced: bool = False
    has_color_glyphs: bool = False
    line_gap: int = 0
    average_advance: float | None = None
    glyph_count: int = 0
    variable_axis_ranges: list[VariableAxis] = field(default_factory=list)
    names: list[FontNameRecord] = field(default_factory=list)
    tables: list[str] = field(default_factory=list)
    warnings: list[str] = field(default_factory=list)

    # Local source path. Optional and never required by the Rust contract.
    path: str | None = None

    # -- construction -----------------------------------------------------

    @classmethod
    def from_bytes(
        cls,
        data: bytes,
        *,
        id: str | None = None,
        source: str = "imported",
        license: str | None = None,
        private: bool = True,
        face_index: int = 0,
    ) -> "FontMetadata":
        fmt = _detect_format(data)
        if fmt in {"woff", "woff2"}:
            raise FontUnsupportedError(
                f"{fmt.upper()} must be decompressed to TTF/OTF before metadata capture"
            )
        if fmt == "bdf":
            raise FontUnsupportedError(
                "BDF fonts do not use SFNT metadata; provide a FontAsset directly"
            )
        return _parse_font(
            data=data,
            path=None,
            id=id,
            source=source,
            license=license,
            private=private,
            face_index=face_index,
        )

    @classmethod
    def from_path(
        cls,
        path: str | Path,
        *,
        id: str | None = None,
        source: str = "imported",
        license: str | None = None,
        private: bool = True,
        face_index: int = 0,
    ) -> "FontMetadata":
        path = Path(path)
        try:
            data = path.read_bytes()
        except OSError as exc:  # pragma: no cover - filesystem dependent
            raise FontError(f"failed to read font file {path}: {exc}") from exc
        return _parse_font(
            data=data,
            path=str(path),
            id=id,
            source=source,
            license=license,
            private=private,
            face_index=face_index,
        )

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "FontMetadata":
        known = {field_name for field_name in cls.__dataclass_fields__}
        kwargs = {key: value for key, value in data.items() if key in known}
        if "path" in data:
            kwargs["path"] = data["path"]
        if "variable_axis_ranges" in kwargs:
            kwargs["variable_axis_ranges"] = [
                axis
                if isinstance(axis, VariableAxis)
                else VariableAxis(
                    tag=str(axis.get("tag", "")),
                    name=axis.get("name"),
                    min=float(axis.get("min", 0.0)),
                    default=float(axis.get("default", 0.0)),
                    max=float(axis.get("max", 0.0)),
                    hidden=bool(axis.get("hidden", False)),
                )
                for axis in kwargs["variable_axis_ranges"]
            ]
        if "names" in kwargs:
            kwargs["names"] = [
                record
                if isinstance(record, FontNameRecord)
                else FontNameRecord(
                    name_id=int(record.get("name_id", 0)),
                    platform=str(record.get("platform", "")),
                    encoding_id=int(record.get("encoding_id", 0)),
                    language_id=int(record.get("language_id", 0)),
                    value=str(record.get("value", "")),
                )
                for record in kwargs["names"]
            ]
        return cls(**kwargs)

    # -- provenance and permissions --------------------------------------

    def with_license(self, license: str | None) -> "FontMetadata":
        self.license = license
        return self

    def with_source(self, source: str) -> "FontMetadata":
        self.source = source
        return self

    def permissions(self) -> EmbeddingPermissions:
        return EmbeddingPermissions.from_fs_type(self.fs_type)

    def license_allowed(self) -> bool:
        return license_is_allowlisted(self.license)

    def can_embed(self) -> bool:
        return self.embedding_allowed and self.license_allowed()

    # -- metric helpers ---------------------------------------------------

    def em_ratio(self, value: int | float | None) -> float | None:
        if value is None or self.units_per_em == 0:
            return None
        return float(value) / float(self.units_per_em)

    def x_height_ratio(self) -> float | None:
        return self.em_ratio(self.x_height)

    def cap_height_ratio(self) -> float | None:
        return self.em_ratio(self.cap_height)

    def ascent_ratio(self) -> float | None:
        return self.em_ratio(self.ascent)

    def descent_ratio(self) -> float | None:
        return self.em_ratio(self.descent)

    def average_advance_ratio(self) -> float | None:
        if self.average_advance is None or self.units_per_em == 0:
            return None
        return self.average_advance / float(self.units_per_em)

    def panose_value(self) -> Panose | None:
        return Panose.parse(self.panose)

    def covers_script(self, script: str) -> bool:
        needle = normalize_script(script)
        return any(normalize_script(value) == needle for value in self.scripts)

    def supports_feature(self, feature: str) -> bool:
        needle = normalize_feature(feature)
        return any(normalize_feature(value) == needle for value in self.features)

    def codepoint_set(self) -> set[int]:
        codepoints: set[int] = set()
        for value in self.glyph_coverage:
            codepoints.update(parse_range(value))
        return codepoints

    # -- serialization ----------------------------------------------------

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {
            "id": self.id,
            "family": self.family,
            "style": self.style,
            "version": self.version,
            "format": self.format,
            "sha256": self.sha256,
            "size_bytes": self.size_bytes,
            "source": self.source,
            "license": self.license,
            "license_url": self.license_url,
            "copyright": self.copyright,
            "embedding_allowed": self.embedding_allowed,
            "subset": self.subset,
            "subset_allowed": self.subset_allowed,
            "fs_type": self.fs_type,
            "bitmap_only": self.bitmap_only,
            "private": self.private,
            "classification": self.classification,
            "weight": self.weight,
            "width": self.width,
            "width_name": self.width_name,
            "slant": self.slant,
            "is_variable": self.is_variable,
            "is_monospaced": self.is_monospaced,
            "has_color_glyphs": self.has_color_glyphs,
            "units_per_em": self.units_per_em,
            "ascent": self.ascent,
            "descent": self.descent,
            "line_gap": self.line_gap,
            "x_height": self.x_height,
            "cap_height": self.cap_height,
            "average_advance": self.average_advance,
            "glyph_count": self.glyph_count,
            "panose": self.panose,
            "os2_family_class": self.os2_family_class,
            "variable_axes": dict(self.variable_axes),
            "variable_axis_ranges": [axis.to_dict() for axis in self.variable_axis_ranges],
            "glyph_coverage": list(self.glyph_coverage),
            "scripts": list(self.scripts),
            "features": list(self.features),
            "names": [record.to_dict() for record in self.names],
            "tables": list(self.tables),
            "warnings": list(self.warnings),
        }
        if self.path is not None:
            data["path"] = self.path
        return data


# ---------------------------------------------------------------------------
# Intent, traits, targets, reports
# ---------------------------------------------------------------------------


@dataclass
class FontRef:
    id: str | None = None
    family: str = ""
    style: str | None = None
    sha256: str | None = None
    source: str | None = None

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "FontRef":
        return cls(
            id=data.get("id"),
            family=data.get("family", ""),
            style=data.get("style"),
            sha256=data.get("sha256"),
            source=data.get("source"),
        )

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {"family": self.family}
        for key in ("id", "style", "sha256", "source"):
            value = getattr(self, key)
            if value is not None:
                data[key] = value
        return data


@dataclass
class FontTraits:
    weight: int | None = None
    width: str | None = None
    slant: str | None = None
    x_height: float | None = None
    cap_height: float | None = None
    contrast: str | None = None
    roundness: str | None = None
    aperture: str | None = None
    panose: str | None = None
    units_per_em: int | None = None
    ascent: int | None = None
    descent: int | None = None
    average_advance: float | None = None

    @classmethod
    def from_dict(cls, data: Mapping[str, Any] | None) -> "FontTraits":
        if not data:
            return cls()
        known = {field_name for field_name in cls.__dataclass_fields__}
        return cls(**{key: value for key, value in data.items() if key in known})

    def to_dict(self) -> dict[str, Any]:
        return {
            key: value
            for key, value in (
                ("weight", self.weight),
                ("width", self.width),
                ("slant", self.slant),
                ("x_height", self.x_height),
                ("cap_height", self.cap_height),
                ("contrast", self.contrast),
                ("roundness", self.roundness),
                ("aperture", self.aperture),
                ("panose", self.panose),
                ("units_per_em", self.units_per_em),
                ("ascent", self.ascent),
                ("descent", self.descent),
                ("average_advance", self.average_advance),
            )
            if value is not None
        }


@dataclass
class TargetFontProfile:
    target: str = ""
    formats: list[str] = field(default_factory=list)
    dpi: int | None = None
    color_depth: int | None = None
    bpp: int | None = None
    cell_width: int | None = None
    cell_height: int | None = None
    antialias: bool | None = None
    hinting: bool | None = None
    max_bytes: int | None = None
    scripts: list[str] = field(default_factory=list)
    fallback_family: str | None = None

    def __post_init__(self) -> None:
        self.formats = [str(value) for value in self.formats]

    @classmethod
    def from_dict(cls, data: Mapping[str, Any] | None) -> "TargetFontProfile":
        if not data:
            return cls()
        known = {field_name for field_name in cls.__dataclass_fields__}
        return cls(**{key: value for key, value in data.items() if key in known})

    def supports_format(self, format: str) -> bool:
        if not self.formats:
            return True
        return any(formats_compatible(candidate, format) for candidate in self.formats)

    def raster_only(self) -> bool:
        if not self.formats:
            return self.bpp is not None
        return all(is_bitmap_format(value) for value in self.formats)

    def fits_bytes(self, size_bytes: int) -> bool:
        return self.max_bytes is None or size_bytes <= self.max_bytes

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {
            "target": self.target,
            "formats": list(self.formats),
        }
        for key in (
            "dpi",
            "color_depth",
            "bpp",
            "cell_width",
            "cell_height",
            "antialias",
            "hinting",
            "max_bytes",
            "scripts",
            "fallback_family",
        ):
            value = getattr(self, key)
            if value is not None and value != []:
                data[key] = value
        return data


@dataclass
class FontIntent:
    id: str = ""
    role: str = ""
    classification: str = ""
    traits: FontTraits = field(default_factory=FontTraits)
    scripts: list[str] = field(default_factory=list)
    features: list[str] = field(default_factory=list)
    target: TargetFontProfile | None = None
    fallbacks: list[FontRef] = field(default_factory=list)

    @classmethod
    def new(cls, role: str, classification: str) -> "FontIntent":
        return cls(role=role, classification=classification)

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "FontIntent":
        return cls(
            id=data.get("id", ""),
            role=data.get("role", ""),
            classification=data.get("classification", ""),
            traits=FontTraits.from_dict(data.get("traits")),
            scripts=list(data.get("scripts") or []),
            features=list(data.get("features") or []),
            target=(
                TargetFontProfile.from_dict(data.get("target"))
                if data.get("target") is not None
                else None
            ),
            fallbacks=[FontRef.from_dict(value) for value in data.get("fallbacks") or []]
            if all(isinstance(value, Mapping) for value in data.get("fallbacks") or [])
            else [FontRef(family=str(value)) for value in data.get("fallbacks") or []],
        )

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {
            "id": self.id,
            "role": self.role,
            "classification": self.classification,
            "traits": self.traits.to_dict(),
            "scripts": list(self.scripts),
            "features": list(self.features),
        }
        if self.target is not None:
            data["target"] = self.target.to_dict()
        if self.fallbacks:
            # The v1 JSON schema models fallbacks as family strings; the richer
            # FontRef form is retained on the dataclass for matching/reporting.
            data["fallbacks"] = [
                ref.family or ref.id or "" for ref in self.fallbacks
            ]
        return data


@dataclass
class TypographyRef:
    intent: str = ""
    size_px: float | None = None
    size_pt: float | None = None
    line_height: float | None = None
    tracking: float | None = None
    weight: int | None = None
    features: list[str] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "TypographyRef":
        known = {field_name for field_name in cls.__dataclass_fields__}
        return cls(**{key: value for key, value in data.items() if key in known})

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {"intent": self.intent, "features": list(self.features)}
        for key in ("size_px", "size_pt", "line_height", "tracking", "weight"):
            value = getattr(self, key)
            if value is not None:
                data[key] = value
        return data


@dataclass
class ScoreBreakdown:
    role: float = 0.0
    panose: float = 0.0
    metrics: float = 0.0
    coverage: float = 0.0
    features: float = 0.0
    format: float = 0.0
    total: float = 0.0

    def to_dict(self) -> dict[str, float]:
        return {
            "role": self.role,
            "panose": self.panose,
            "metrics": self.metrics,
            "coverage": self.coverage,
            "features": self.features,
            "format": self.format,
            "total": self.total,
        }


@dataclass
class FontMatch:
    font: FontMetadata
    score: float
    breakdown: ScoreBreakdown
    missing_scripts: list[str] = field(default_factory=list)
    missing_features: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "font": self.font.to_dict(),
            "score": self.score,
            "breakdown": self.breakdown.to_dict(),
            "missing_scripts": list(self.missing_scripts),
            "missing_features": list(self.missing_features),
        }


@dataclass
class FontTranslationReport:
    """Export-only font loss report."""

    source_font: str = ""
    target_font: str = ""
    action: str = "unavailable"
    shape_score: float | None = None
    metrics: dict[str, Any] | None = None
    dropped_glyphs: list[str] = field(default_factory=list)
    dropped_features: list[str] = field(default_factory=list)
    license_ok: bool = False
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        """Return a schema-valid ``FontTranslationReport`` mapping.

        The Rust model allows an empty ``target_font`` for ``unavailable``, but
        the JSON schema requires non-empty strings. Filling a descriptive
        placeholder here keeps the report useful and schema-valid without
        changing the in-memory matching result.
        """

        source_font = self.source_font or "unknown"
        target_font = self.target_font or (
            "unavailable" if self.action == "unavailable" else source_font
        )
        data: dict[str, Any] = {
            "source_font": source_font,
            "target_font": target_font,
            "action": self.action,
            "dropped_glyphs": list(self.dropped_glyphs),
            "dropped_features": list(self.dropped_features),
            "license_ok": self.license_ok,
            "warnings": list(self.warnings),
        }
        if self.shape_score is not None:
            data["shape_score"] = self.shape_score
        if self.metrics is not None:
            data["metrics"] = dict(self.metrics)
        return data


# ---------------------------------------------------------------------------
# Normalization and licence helpers
# ---------------------------------------------------------------------------


def normalize_format(format: str) -> str:
    value = str(format).strip().lower()
    return _FORMAT_ALIASES.get(value, value)


def formats_compatible(left: str, right: str) -> bool:
    return normalize_format(left) == normalize_format(right)


def is_bitmap_format(format: str) -> bool:
    return normalize_format(format) in _BITMAP_FORMATS


def normalize_script(script: str) -> str:
    return re.sub(r"[_\s]+", "-", str(script).strip().lower())


def canonical_feature_name(feature: str) -> str:
    normalized = re.sub(r"[-\s]+", "_", str(feature).strip().lower())
    return _FEATURE_ALIASES.get(normalized, normalized)


def normalize_feature(feature: str) -> str:
    return canonical_feature_name(feature)


def license_is_allowlisted(license: str | None) -> bool:
    """Whether a licence string is on the Ozma redistribution allowlist.

    Allowed families are MIT, BSD, ISC, Apache-2.0, SIL OFL, CC0, public domain,
    and Unlicense. GPL/AGPL/LGPL, non-commercial, personal-use-only, proprietary,
    and unknown licences are rejected. This is intentionally conservative:
    missing or opaque prose is rejected unless it names an allowed family.
    """

    if license is None:
        return False
    normalized = license.strip().lower()
    if not normalized:
        return False
    reject = (
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
    )
    if any(token in normalized for token in reject):
        return False
    allow_tokens = {"mit", "bsd", "isc", "apache", "ofl", "cc0", "unlicense"}
    tokens = [token for token in re.split(r"[^a-z0-9]+", normalized) if token]
    return (
        any(token in allow_tokens for token in tokens)
        or "open font license" in normalized
        or "public domain" in normalized
    )


# ---------------------------------------------------------------------------
# Matching (mirrors ozma-fonts/src/matching.rs)
# ---------------------------------------------------------------------------


def panose_distance(left: str, right: str) -> float | None:
    left_value = Panose.parse(left)
    right_value = Panose.parse(right)
    if left_value is None or right_value is None:
        return None
    if left_value.family_kind != right_value.family_kind:
        return 1.0
    if left_value.is_empty or right_value.is_empty:
        return 0.5
    weights = (1.0, 0.18, 0.18, 0.12, 0.10, 0.12, 0.08, 0.08, 0.07, 0.07)
    total_weight = sum(weights)
    distance = 0.0
    for index, weight in enumerate(weights):
        difference = abs(left_value.values[index] - right_value.values[index]) / 15.0
        distance += difference * weight
    return min(1.0, max(0.0, distance / total_weight))


def match_fonts(
    intent: FontIntent | Mapping[str, Any],
    candidates: Sequence[FontMetadata | Mapping[str, Any]],
) -> list[FontMatch]:
    """Score all candidates against an intent, with stable deterministic order."""

    intent = _as_intent(intent)
    fonts = [_as_metadata(candidate) for candidate in candidates]
    matches: list[FontMatch] = []
    for candidate in fonts:
        if not passes_hard_filters(intent, candidate):
            continue
        breakdown = score_candidate(intent, candidate)
        if breakdown is None:
            continue
        matches.append(
            FontMatch(
                font=candidate,
                score=breakdown.total,
                breakdown=breakdown,
                missing_scripts=missing_scripts(intent, candidate),
                missing_features=missing_features(intent, candidate),
            )
        )
    matches.sort(
        key=lambda match: (
            -match.score,
            match.font.family.lower(),
            match.font.style,
            match.font.sha256,
            match.font.id,
        )
    )
    return matches


def match_font(
    intent: FontIntent | Mapping[str, Any],
    candidates: Sequence[FontMetadata | Mapping[str, Any]],
) -> list[FontMatch]:
    """Alias for :func:`match_fonts` matching the Rust function name."""

    return match_fonts(intent, candidates)


def choose_font(
    intent: FontIntent | Mapping[str, Any],
    candidates: Sequence[FontMetadata | Mapping[str, Any]],
) -> FontMatch | None:
    matches = match_fonts(intent, candidates)
    return matches[0] if matches else None


def score_candidate(
    intent: FontIntent | Mapping[str, Any],
    candidate: FontMetadata | Mapping[str, Any],
) -> ScoreBreakdown | None:
    intent = _as_intent(intent)
    candidate = _as_metadata(candidate)

    role = role_score(intent, candidate)
    if intent.traits.panose is not None:
        distance = (
            panose_distance(intent.traits.panose, candidate.panose)
            if candidate.panose is not None
            else None
        )
        shape = (
            1.0 - distance
            if distance is not None
            else trait_shape_score(intent.traits, candidate)
        )
    else:
        shape = trait_shape_score(intent.traits, candidate)
    metrics = metric_score(intent.traits, candidate)
    coverage = script_coverage(intent.scripts, candidate)
    features = feature_coverage(intent.features, candidate)
    format_score_value = _format_score(intent.target, candidate)

    total = (
        role * WEIGHT_ROLE
        + shape * WEIGHT_PANOSE
        + metrics * WEIGHT_METRICS
        + coverage * WEIGHT_COVERAGE
        + features * WEIGHT_FEATURES
        + format_score_value * WEIGHT_FORMAT
    )
    return ScoreBreakdown(
        role=_round_score(role),
        panose=_round_score(shape),
        metrics=_round_score(metrics),
        coverage=_round_score(coverage),
        features=_round_score(features),
        format=_round_score(format_score_value),
        total=_round_score(total),
    )


def plan_export(
    intent: FontIntent | Mapping[str, Any],
    original: FontMetadata | Mapping[str, Any] | None,
    candidates: Sequence[FontMetadata | Mapping[str, Any]],
    target: TargetFontProfile | Mapping[str, Any],
) -> FontTranslationReport:
    """Plan an export and return the loss report.

    Decision order matches Rust: exact, subset, rasterise, substitute, then
    unavailable. Every path returns a report.
    """

    report, _chosen = _plan_export_with_font(intent, original, candidates, target)
    return report


def _plan_export_with_font(
    intent: FontIntent | Mapping[str, Any],
    original: FontMetadata | Mapping[str, Any] | None,
    candidates: Sequence[FontMetadata | Mapping[str, Any]],
    target: TargetFontProfile | Mapping[str, Any],
) -> tuple[FontTranslationReport, FontMetadata | None]:
    intent = _as_intent(intent)
    target = _as_target(target)
    original_font = _as_metadata(original) if original is not None else None
    fonts = [_as_metadata(candidate) for candidate in candidates]

    warnings: list[str] = []
    report = FontTranslationReport(
        source_font=original_font.family if original_font is not None else ""
    )

    source_license_ok = (
        original_font is not None and original_font.license_allowed()
    )
    source_embed_ok = (
        original_font is not None and original_font.embedding_allowed
    )
    source_bitmap_ok = original_font is not None and original_font.bitmap_only

    if original_font is not None:
        if not source_license_ok:
            warnings.append(
                f"source font '{original_font.family}' has a "
                "non-redistributable or unknown licence"
            )
        if not source_embed_ok and not source_bitmap_ok:
            warnings.append(
                f"source font '{original_font.family}' does not permit embedding "
                "(OS/2 fsType)"
            )

    if original_font is not None:
        format_ok = target.supports_format(original_font.format)
        if (
            source_license_ok
            and target.raster_only()
            and not format_ok
            and (source_embed_ok or source_bitmap_ok)
        ):
            _fill_direct_report(
                report,
                "rasterized",
                original_font,
                original_font,
                intent,
                warnings,
            )
            return report, original_font
        if source_license_ok and source_embed_ok and format_ok:
            oversized = not target.fits_bytes(original_font.size_bytes)
            if not oversized:
                _fill_direct_report(
                    report, "exact", original_font, original_font, intent, warnings
                )
                return report, original_font
            if original_font.subset_allowed:
                _fill_direct_report(
                    report, "subset", original_font, original_font, intent, warnings
                )
                return report, original_font
            warnings.append(
                f"source font '{original_font.family}' exceeds the target size "
                "budget and subsetting is not permitted"
            )

    match_intent = _as_intent(intent)
    match_intent.target = _as_target(target)
    if original_font is not None:
        _preserve_source_metrics(match_intent.traits, original_font)

    matches = match_fonts(match_intent, fonts)
    rejected_oversize: str | None = None
    chosen: tuple[FontMatch, bool] | None = None
    for candidate in matches:
        font = candidate.font
        license_ok = font.license_allowed()
        embedding_ok = font.embedding_allowed or (
            target.raster_only() and font.bitmap_only
        )
        if not license_ok or not embedding_ok:
            continue
        if not target.raster_only() and not target.supports_format(font.format):
            continue
        oversized = (
            not target.raster_only() and not target.fits_bytes(font.size_bytes)
        )
        if oversized and not font.subset_allowed:
            rejected_oversize = font.family
            continue
        chosen = (candidate, oversized)
        break

    if chosen is None:
        if rejected_oversize is not None:
            warnings.append(
                f"candidate '{rejected_oversize}' exceeds the target size budget "
                "and cannot be subset"
            )
        report.action = "unavailable"
        report.license_ok = False
        report.dropped_glyphs = [
            f"script:{script}" for script in _required_scripts(intent)
        ]
        report.dropped_features = _normalized_features(intent.features)
        report.warnings = warnings
        return report, None

    matched, oversized = chosen
    font = matched.font
    rasterized = target.raster_only() and not target.supports_format(font.format)
    if rasterized:
        action = "rasterized"
    elif oversized:
        action = "subset"
    else:
        action = "substituted"

    if action == "substituted":
        warnings.append(
            f"substituted '{report.source_font}' with '{font.family}' "
            f"(score {_format_score_text(matched.score)})"
        )

    report.action = action
    report.target_font = font.family
    report.shape_score = matched.score
    report.metrics = _metric_map(original_font, font)
    report.dropped_glyphs = [
        f"script:{script}" for script in missing_scripts(intent, font)
    ]
    report.dropped_features = missing_features(intent, font)
    report.license_ok = font.can_embed() or font.bitmap_only
    report.warnings = warnings
    return report, font


def candidate_supports_target(
    target: TargetFontProfile | Mapping[str, Any],
    candidate: FontMetadata | Mapping[str, Any],
) -> bool:
    return _as_target(target).supports_format(_as_metadata(candidate).format)


# ---------------------------------------------------------------------------
# Score components
# ---------------------------------------------------------------------------


def passes_hard_filters(intent: FontIntent, candidate: FontMetadata) -> bool:
    intended = _normalize_classification(intent.classification)
    actual = _normalize_classification(candidate.classification)
    if intended and actual and intended != actual:
        return False
    role = intent.role.strip().lower()
    if role == "icon":
        return not actual or actual == "icon"
    if role == "code":
        return not actual or actual == "mono" or candidate.is_monospaced
    return True


def role_score(intent: FontIntent, candidate: FontMetadata) -> float:
    classification = _normalize_classification(candidate.classification)
    role = intent.role.strip().lower()
    if role == "":
        return 0.75
    if role == "icon":
        return 1.0 if classification == "icon" else 0.0
    if role == "code":
        return 1.0 if classification == "mono" or candidate.is_monospaced else 0.0
    if role == "numeric":
        return {"mono": 1.0, "sans": 0.8, "serif": 0.8, "display": 0.6}.get(
            classification, 0.4
        )
    if role in {"display", "brand"}:
        return {"display": 1.0, "sans": 0.75, "serif": 0.65}.get(classification, 0.4)
    if role in {"body", "label", "caption"}:
        return {
            "sans": 1.0,
            "serif": 0.85,
            "mono": 0.6,
            "display": 0.7,
        }.get(classification, 0.5)
    return 0.75


def trait_shape_score(traits: FontTraits, candidate: FontMetadata) -> float:
    score = 0.0
    weight = 0.0

    if traits.weight is not None:
        difference = abs(candidate.weight - traits.weight) / 800.0
        score += max(0.0, min(1.0, 1.0 - difference)) * 0.25
        weight += 0.25
    if traits.width is not None:
        target_width = _width_number(traits.width)
        if target_width is not None:
            difference = abs(candidate.width - target_width) / 8.0
            score += max(0.0, min(1.0, 1.0 - difference)) * 0.20
            weight += 0.20
    if traits.slant is not None:
        score += _slant_score(traits.slant, candidate.slant) * 0.15
        weight += 0.15
    if traits.contrast is not None:
        panose = candidate.panose_value()
        candidate_contrast = _panose_contrast(panose) if panose else None
        if candidate_contrast is not None:
            score += _normalized_eq_score(traits.contrast, candidate_contrast) * 0.15
            weight += 0.15
    if traits.roundness is not None:
        panose = candidate.panose_value()
        candidate_roundness = _panose_roundness(panose) if panose else None
        if candidate_roundness is not None:
            score += (
                _normalized_eq_score(traits.roundness, candidate_roundness) * 0.10
            )
            weight += 0.10
    if traits.x_height is not None:
        actual = candidate.x_height_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(traits.x_height, traits.units_per_em),
                    actual,
                )
                * 0.075
            )
            weight += 0.075
    if traits.cap_height is not None:
        actual = candidate.cap_height_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(traits.cap_height, traits.units_per_em),
                    actual,
                )
                * 0.075
            )
            weight += 0.075

    if weight == 0.0:
        return 1.0
    return max(0.0, min(1.0, score / weight))


def metric_score(traits: FontTraits, candidate: FontMetadata) -> float:
    score = 0.0
    weight = 0.0

    if traits.units_per_em is not None and candidate.units_per_em > 0:
        score += _closeness(float(traits.units_per_em), float(candidate.units_per_em)) * 0.05
        weight += 0.05
    if traits.x_height is not None:
        actual = candidate.x_height_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(traits.x_height, traits.units_per_em),
                    actual,
                )
                * 0.25
            )
            weight += 0.25
    if traits.cap_height is not None:
        actual = candidate.cap_height_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(traits.cap_height, traits.units_per_em),
                    actual,
                )
                * 0.20
            )
            weight += 0.20
    if traits.ascent is not None:
        actual = candidate.ascent_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(float(traits.ascent), traits.units_per_em),
                    actual,
                )
                * 0.20
            )
            weight += 0.20
    if traits.descent is not None:
        actual = candidate.descent_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(float(traits.descent), traits.units_per_em),
                    actual,
                )
                * 0.15
            )
            weight += 0.15
    if traits.average_advance is not None:
        actual = candidate.average_advance_ratio()
        if actual is not None:
            score += (
                _closeness(
                    _normalize_intent_ratio(
                        traits.average_advance, traits.units_per_em
                    ),
                    actual,
                )
                * 0.20
            )
            weight += 0.20

    if weight == 0.0:
        return 1.0
    return max(0.0, min(1.0, score / weight))


def script_coverage(required: Iterable[str], candidate: FontMetadata) -> float:
    scripts = _normalized_scripts(required)
    if not scripts:
        return 1.0
    matched = sum(1 for script in scripts if candidate.covers_script(script))
    return matched / len(scripts)


def feature_coverage(required: Iterable[str], candidate: FontMetadata) -> float:
    features = _normalized_features(required)
    if not features:
        return 1.0
    matched = sum(1 for feature in features if candidate.supports_feature(feature))
    return matched / len(features)


def missing_scripts(intent: FontIntent, candidate: FontMetadata) -> list[str]:
    return [
        script
        for script in _normalized_scripts(intent.scripts)
        if not candidate.covers_script(script)
    ]


def missing_features(intent: FontIntent, candidate: FontMetadata) -> list[str]:
    return [
        feature
        for feature in _normalized_features(intent.features)
        if not candidate.supports_feature(feature)
    ]


# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------


def _as_metadata(value: FontMetadata | Mapping[str, Any]) -> FontMetadata:
    if isinstance(value, FontMetadata):
        return value
    return FontMetadata.from_dict(value)


def _as_intent(value: FontIntent | Mapping[str, Any]) -> FontIntent:
    if isinstance(value, FontIntent):
        return value
    return FontIntent.from_dict(value)


def _as_target(
    value: TargetFontProfile | Mapping[str, Any],
) -> TargetFontProfile:
    if isinstance(value, TargetFontProfile):
        return value
    return TargetFontProfile.from_dict(value)


def _fill_direct_report(
    report: FontTranslationReport,
    action: str,
    font: FontMetadata,
    original: FontMetadata | None,
    intent: FontIntent,
    warnings: list[str],
) -> None:
    report.action = action
    report.target_font = font.family
    report.metrics = _metric_map(original, font)
    report.dropped_glyphs = [
        f"script:{script}" for script in missing_scripts(intent, font)
    ]
    report.dropped_features = missing_features(intent, font)
    report.license_ok = True
    report.warnings = warnings


def _normalize_classification(value: str) -> str:
    normalized = value.strip().lower()
    if normalized in {"sans-serif", "sans serif", "grotesque"}:
        return "sans"
    if normalized in {"monospaced", "monospace"}:
        return "mono"
    if normalized == "cursive":
        return "script"
    return normalized


def _width_number(width: str) -> float | None:
    normalized = width.strip().lower().replace("_", "-")
    if normalized in _WIDTH_NUMBERS:
        return _WIDTH_NUMBERS[normalized]
    try:
        return float(normalized)
    except ValueError:
        return None


def _slant_score(target: str, actual: str) -> float:
    target = target.strip().lower()
    actual = actual.strip().lower()
    if target == actual:
        return 1.0
    if target in {"italic", "oblique"} and actual in {"italic", "oblique"}:
        return 0.8
    if target == "upright" or actual == "upright":
        return 0.15
    return 0.5


def _panose_contrast(panose: Panose) -> str | None:
    value = panose.stroke_variation
    if value == 0:
        return None
    if value <= 3:
        return "low"
    if value <= 6:
        return "medium"
    return "high"


def _panose_roundness(panose: Panose) -> str | None:
    if panose.family_kind != 2:
        return None
    return {
        11: "grotesque",
        12: "humanist",
        14: "humanist",
        13: "geometric",
        15: "geometric",
    }.get(panose.serif_style)


def _normalized_eq_score(left: str, right: str) -> float:
    return 1.0 if left.strip().lower() == right.strip().lower() else 0.25


def _normalize_intent_ratio(value: float, units_per_em: int | None) -> float:
    if units_per_em is not None and units_per_em > 0 and abs(value) > 2.0:
        return value / float(units_per_em)
    return value


def _closeness(left: float, right: float) -> float:
    span = max(abs(left), abs(right), 0.001)
    return max(0.0, min(1.0, 1.0 - abs(left - right) / span))


def _format_score(
    target: TargetFontProfile | None, candidate: FontMetadata
) -> float:
    if target is None:
        return 1.0
    if target.supports_format(candidate.format):
        return 1.0
    if target.raster_only():
        return 0.9
    return 0.0


def _normalized_scripts(values: Iterable[str]) -> list[str]:
    return sorted(
        {
            normalize_script(value)
            for value in values
            if normalize_script(value)
        }
    )


def _normalized_features(values: Iterable[str]) -> list[str]:
    return sorted(
        {
            normalize_feature(value)
            for value in values
            if normalize_feature(value)
        }
    )


def _required_scripts(intent: FontIntent) -> list[str]:
    return _normalized_scripts(intent.scripts)


def _preserve_source_metrics(traits: FontTraits, source: FontMetadata) -> None:
    if traits.units_per_em is None:
        traits.units_per_em = source.units_per_em
    if traits.x_height is None:
        traits.x_height = source.x_height_ratio()
    if traits.cap_height is None:
        traits.cap_height = source.cap_height_ratio()
    if traits.ascent is None:
        traits.ascent = source.ascent
    if traits.descent is None:
        traits.descent = source.descent
    if traits.average_advance is None:
        traits.average_advance = source.average_advance_ratio()


def _metric_map(
    source: FontMetadata | None, target: FontMetadata | None
) -> dict[str, Any]:
    metrics: dict[str, Any] = {}
    if source is not None:
        metrics["source_units_per_em"] = source.units_per_em
        for key, value in (
            ("source_x_height_ratio", source.x_height_ratio()),
            ("source_cap_height_ratio", source.cap_height_ratio()),
            ("source_ascent_ratio", source.ascent_ratio()),
            ("source_descent_ratio", source.descent_ratio()),
            ("source_average_advance_ratio", source.average_advance_ratio()),
        ):
            if value is not None:
                metrics[key] = value
    if target is not None:
        metrics["target_units_per_em"] = target.units_per_em
        for key, value in (
            ("target_x_height_ratio", target.x_height_ratio()),
            ("target_cap_height_ratio", target.cap_height_ratio()),
            ("target_ascent_ratio", target.ascent_ratio()),
            ("target_descent_ratio", target.descent_ratio()),
            ("target_average_advance_ratio", target.average_advance_ratio()),
        ):
            if value is not None:
                metrics[key] = value
        if source is not None and source.units_per_em > 0:
            metrics["size_scale"] = target.units_per_em / source.units_per_em
    return metrics


def _round_score(value: float) -> float:
    clamped = max(0.0, min(1.0, value))
    return math.floor(clamped * 10_000.0 + 0.5) / 10_000.0


def _format_score_text(value: float) -> str:
    return f"{value:.4f}"


def parse_range(value: str | int | tuple[int, int]) -> set[int]:
    """Parse ``U+0041``, ``U+0041-U+005A``, ``0x41-0x5A``, or an integer."""

    if isinstance(value, int):
        return {value}
    if isinstance(value, tuple) and len(value) == 2:
        start, end = int(value[0]), int(value[1])
        return set(range(min(start, end), max(start, end) + 1))
    text = str(value).strip()
    if not text:
        return set()
    if "-" in text:
        left, right = text.split("-", 1)
        return set(range(_parse_codepoint(left), _parse_codepoint(right) + 1))
    return {_parse_codepoint(text)}


def codepoints_from_glyphs(glyphs: Iterable[str | int | tuple[int, int]]) -> set[int]:
    codepoints: set[int] = set()
    if isinstance(glyphs, str):
        glyphs = [glyphs]
    for glyph in glyphs:
        if isinstance(glyph, int):
            codepoints.add(glyph)
        elif isinstance(glyph, tuple):
            codepoints.update(parse_range(glyph))
        elif isinstance(glyph, str):
            text = glyph.strip()
            if len(text) == 1:
                codepoints.add(ord(text))
            elif text.startswith(("U+", "u+", "0x", "0X")) or "-" in text:
                codepoints.update(parse_range(text))
            else:
                codepoints.update(ord(character) for character in text)
        else:  # pragma: no cover - defensive
            raise TypeError(f"unsupported glyph selector: {glyph!r}")
    return codepoints


def _parse_codepoint(value: str) -> int:
    text = value.strip()
    if text.lower().startswith("u+"):
        return int(text[2:], 16)
    if text.lower().startswith("0x"):
        return int(text, 16)
    if len(text) == 1 and not text.isdigit():
        return ord(text)
    return int(text, 10)


# ---------------------------------------------------------------------------
# Font parsing
# ---------------------------------------------------------------------------


def _fonttools() -> tuple[Any, Any]:
    try:
        from fontTools.ttLib import TTFont
        from io import BytesIO
    except ImportError as exc:  # pragma: no cover - dependency missing
        raise FontError(
            "fontTools is required for font metadata extraction "
            "(install the 'fonttools' package)"
        ) from exc
    return TTFont, BytesIO


def _detect_format(data: bytes) -> str:
    magic = data[:4]
    if magic in (b"\x00\x01\x00\x00", b"true"):
        return "ttf"
    if magic == b"OTTO":
        return "otf"
    if magic == b"ttcf":
        return "ttc"
    if magic == b"wOFF":
        return "woff"
    if magic == b"wOF2":
        return "woff2"
    if data.startswith(b"STARTFONT "):
        return "bdf"
    raise FontUnsupportedError(f"unrecognized font magic {magic.hex()}")


def _sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _parse_font(
    *,
    data: bytes,
    path: str | None,
    id: str | None,
    source: str,
    license: str | None,
    private: bool,
    face_index: int,
) -> FontMetadata:
    TTFont, BytesIO = _fonttools()
    fmt = _detect_format(data)
    try:
        font = TTFont(BytesIO(data), fontNumber=face_index, lazy=True)
    except Exception as exc:  # pragma: no cover - malformed input dependent
        raise FontError(f"failed to parse font: {exc}") from exc

    try:
        sha256 = _sha256_hex(data)
        names = _collect_names(font)
        family = _pick_name(names, (16, 1)) or "Unknown"
        style = _pick_name(names, (17, 2)) or "Regular"
        version = _pick_name(names, (5,))
        copyright_text = _pick_name(names, (0,))
        license_description = _pick_name(names, (13,))
        license_url = _pick_name(names, (14,))

        os2 = font["OS/2"] if "OS/2" in font else None
        fs_type = int(os2.fsType) if os2 is not None else None
        permissions = EmbeddingPermissions.from_fs_type(fs_type)
        panose_string = _panose_string(os2)
        panose = Panose.parse(panose_string) if panose_string else None
        family_class = int(os2.sFamilyClass) if os2 is not None else None

        weight = int(getattr(os2, "usWeightClass", 400)) if os2 else 400
        if weight <= 0:
            weight = 400
        width = int(getattr(os2, "usWidthClass", 5)) if os2 else 5
        width_name = _WIDTH_NAMES.get(width, "normal")
        slant = _slant(font, os2)

        codepoints = _collect_codepoints(font)
        scripts = _classify_scripts(codepoints)
        glyph_coverage = _coverage_ranges(codepoints)
        features = _collect_features(font)
        axis_ranges = _collect_variable_axes(font, names)
        variable_axes = {axis.tag: axis.default for axis in axis_ranges}
        tables = _collect_tables(font)
        monospaced = _is_monospaced(font, codepoints)
        classification = _classify_font(family, monospaced, panose)
        units_per_em = int(font["head"].unitsPerEm) if "head" in font else 0
        ascent = int(font["hhea"].ascent) if "hhea" in font else 0
        descent = int(font["hhea"].descent) if "hhea" in font else 0
        line_gap = int(font["hhea"].lineGap) if "hhea" in font else 0
        x_height = _os2_metric(os2, "sxHeight")
        cap_height = _os2_metric(os2, "sCapHeight")
        average_advance = _average_advance(font, codepoints)
        glyph_count = int(font["maxp"].numGlyphs) if "maxp" in font else len(
            font.getGlyphOrder()
        )

        declared_license = license if license is not None else license_description
        warnings: list[str] = []
        if fs_type is None:
            warnings.append(
                "font has no readable OS/2 fsType; embedding is not permitted "
                "by default"
            )
        elif permissions.restricted:
            warnings.append("OS/2 fsType marks the font as restricted")
        elif permissions.preview_print_only:
            warnings.append("OS/2 fsType permits preview/print embedding only")
        if permissions.bitmap_only:
            warnings.append("OS/2 fsType permits bitmap embedding only")
        if permissions.no_subsetting:
            warnings.append("OS/2 fsType forbids subsetting")
        if license is None and license_description is None:
            warnings.append("font declares no licence in its name table")

        asset_id = id or f"font-{sha256[:16]}"
        return FontMetadata(
            id=asset_id,
            family=family,
            style=style,
            version=version,
            format=fmt,
            sha256=sha256,
            source=source,
            license=declared_license,
            embedding_allowed=permissions.embedding_allowed,
            subset_allowed=permissions.subset_allowed,
            units_per_em=units_per_em,
            ascent=ascent,
            descent=descent,
            x_height=x_height,
            cap_height=cap_height,
            panose=panose_string,
            os2_family_class=family_class,
            variable_axes=variable_axes,
            glyph_coverage=glyph_coverage,
            scripts=scripts,
            features=features,
            private=private,
            size_bytes=len(data),
            license_url=license_url,
            copyright=copyright_text,
            subset=False,
            fs_type=fs_type,
            bitmap_only=permissions.bitmap_only,
            classification=classification,
            weight=weight,
            width=width,
            width_name=width_name,
            slant=slant,
            is_variable=bool(axis_ranges),
            is_monospaced=monospaced,
            has_color_glyphs=any(
                table in font for table in ("COLR", "CBDT", "sbix", "SVG ")
            ),
            line_gap=line_gap,
            average_advance=average_advance,
            glyph_count=glyph_count,
            variable_axis_ranges=axis_ranges,
            names=names,
            tables=tables,
            warnings=warnings,
            path=path,
        )
    finally:
        try:
            font.close()
        except Exception:  # pragma: no cover - best effort
            pass


def _collect_names(font: Any) -> list[FontNameRecord]:
    records: list[FontNameRecord] = []
    if "name" not in font:
        return records
    platforms = {0: "unicode", 1: "macintosh", 2: "iso", 3: "windows", 4: "custom"}
    for record in font["name"].names:
        try:
            value = record.toUnicode()
        except Exception:  # pragma: no cover - malformed name record
            continue
        if not value:
            continue
        records.append(
            FontNameRecord(
                name_id=int(record.nameID),
                platform=platforms.get(int(record.platformID), "custom"),
                encoding_id=int(record.platEncID),
                language_id=int(record.langID),
                value=value,
            )
        )
    records.sort(
        key=lambda item: (
            item.name_id,
            item.platform,
            item.encoding_id,
            item.language_id,
            item.value,
        )
    )
    deduped: list[FontNameRecord] = []
    for record in records:
        if not deduped or deduped[-1] != record:
            deduped.append(record)
    return deduped


def _pick_name(records: Sequence[FontNameRecord], ids: Sequence[int]) -> str | None:
    for name_id in ids:
        best: tuple[int, FontNameRecord] | None = None
        for record in records:
            if record.name_id != name_id:
                continue
            score = 0
            if record.platform == "windows" and record.language_id == 0x0409:
                score = 4
            elif record.platform == "windows":
                score = 3
            elif record.platform == "unicode":
                score = 2
            elif record.platform == "macintosh":
                score = 1
            if best is None or score > best[0]:
                best = (score, record)
        if best is not None:
            return best[1].value
    return None


def _panose_string(os2: Any) -> str | None:
    if os2 is None:
        return None
    panose = getattr(os2, "panose", None)
    if panose is None:
        return None
    values = [
        int(getattr(panose, field, 0))
        for field in (
            "bFamilyType",
            "bSerifStyle",
            "bWeight",
            "bProportion",
            "bContrast",
            "bStrokeVariation",
            "bArmStyle",
            "bLetterForm",
            "bMidline",
            "bXHeight",
        )
    ]
    if all(value == 0 for value in values):
        return None
    return " ".join(str(value) for value in values)


def _slant(font: Any, os2: Any) -> str:
    italic = False
    oblique = False
    if "head" in font:
        italic = bool(int(font["head"].macStyle) & 0x0002)
    if os2 is not None:
        selection = int(getattr(os2, "fsSelection", 0))
        italic = italic or bool(selection & 0x0001)
        oblique = bool(selection & 0x0200)
    if italic:
        return "italic"
    if oblique:
        return "oblique"
    return "upright"


def _collect_tables(font: Any) -> list[str]:
    try:
        return sorted(str(tag) for tag in font.reader.tables)
    except Exception:  # pragma: no cover - defensive
        return sorted(
            str(tag) for tag in font.keys() if str(tag) != "GlyphOrder"
        )


def _collect_codepoints(font: Any) -> set[int]:
    codepoints: set[int] = set()
    if "cmap" not in font:
        return codepoints
    for table in font["cmap"].tables:
        try:
            unicode_table = table.isUnicode()
        except Exception:  # pragma: no cover - legacy table
            unicode_table = int(table.platformID) == 0
        if not unicode_table:
            continue
        for codepoint in table.cmap:
            if isinstance(codepoint, int) and 0 <= codepoint <= 0x10FFFF:
                codepoints.add(codepoint)
    return codepoints


def _average_advance(font: Any, codepoints: set[int]) -> float | None:
    if "hmtx" not in font:
        return None
    cmap = font.getBestCmap() or {}
    total = 0
    count = 0
    for codepoint in sorted(codepoints)[:8192]:
        glyph_name = cmap.get(codepoint)
        if glyph_name is None:
            continue
        try:
            advance = int(font["hmtx"][glyph_name][0])
        except Exception:  # pragma: no cover - incomplete hmtx
            continue
        total += advance
        count += 1
    if not count:
        return None
    return total / count


def _coverage_ranges(codepoints: set[int]) -> list[str]:
    if not codepoints:
        return []
    ordered = sorted(codepoints)
    ranges: list[str] = []
    start = ordered[0]
    end = start
    for codepoint in ordered[1:]:
        if codepoint == end + 1:
            end = codepoint
        else:
            ranges.append(_format_range(start, end))
            start = codepoint
            end = codepoint
    ranges.append(_format_range(start, end))
    return ranges


def _format_range(start: int, end: int) -> str:
    if start == end:
        return f"U+{start:04X}"
    return f"U+{start:04X}-U+{end:04X}"


def _classify_scripts(codepoints: set[int]) -> list[str]:
    found: list[str] = []
    for name, ranges in _SCRIPT_RANGES:
        if any(
            start <= codepoint <= end
            for codepoint in codepoints
            for start, end in ranges
        ):
            found.append(name)
    return sorted(found)


def _collect_features(font: Any) -> list[str]:
    features: set[str] = set()
    for table_name in ("GSUB", "GPOS"):
        if table_name not in font:
            continue
        try:
            records = font[table_name].table.FeatureList.FeatureRecord
        except Exception:  # pragma: no cover - malformed layout table
            continue
        for record in records:
            features.add(canonical_feature_name(str(record.FeatureTag)))
    if "kern" in font:
        features.add("kerning")
    return sorted(features)


def _collect_variable_axes(
    font: Any, names: Sequence[FontNameRecord]
) -> list[VariableAxis]:
    if "fvar" not in font:
        return []
    axes: list[VariableAxis] = []
    for axis in font["fvar"].axes:
        tag = str(axis.axisTag)
        hidden = bool(getattr(axis, "hidden", False)) or bool(
            int(getattr(axis, "flags", 0)) & 0x0001
        )
        axes.append(
            VariableAxis(
                tag=tag,
                name=_pick_name(names, (int(axis.axisNameID),)),
                min=float(axis.minValue),
                default=float(axis.defaultValue),
                max=float(axis.maxValue),
                hidden=hidden,
            )
        )
    axes.sort(key=lambda axis: axis.tag)
    return axes


def _is_monospaced(font: Any, codepoints: set[int]) -> bool:
    if "post" in font and int(getattr(font["post"], "isFixedPitch", 0)):
        return True
    if "hmtx" not in font:
        return False
    cmap = font.getBestCmap() or {}
    advances: set[int] = set()
    for codepoint in sorted(codepoints)[:256]:
        glyph_name = cmap.get(codepoint)
        if glyph_name is None:
            continue
        try:
            advances.add(int(font["hmtx"][glyph_name][0]))
        except Exception:  # pragma: no cover - incomplete hmtx
            continue
        if len(advances) > 1:
            return False
    return bool(advances)


def _os2_metric(os2: Any, name: str) -> int | None:
    if os2 is None:
        return None
    try:
        value = getattr(os2, name, None)
    except Exception:  # pragma: no cover - older OS/2 versions
        return None
    if value is None:
        return None
    return int(value)


def _classify_font(
    family: str, monospaced: bool, panose: Panose | None
) -> str:
    family_lower = family.lower()
    if (
        monospaced
        or "mono" in family_lower
        or "code" in family_lower
        or "console" in family_lower
    ):
        return "mono"
    if (
        "icon" in family_lower
        or "symbol" in family_lower
        or "awesome" in family_lower
        or "emoji" in family_lower
    ):
        return "icon"
    if (
        "script" in family_lower
        or "hand" in family_lower
        or "cursive" in family_lower
    ):
        return "script"
    if panose is not None:
        if panose.family_kind == 3:
            return "script"
        if panose.family_kind == 4:
            return "display"
        if panose.family_kind == 5:
            return "icon"
    if "sans" in family_lower:
        return "sans"
    if "serif" in family_lower:
        return "serif"
    if "display" in family_lower or "decorative" in family_lower:
        return "display"
    if panose is not None:
        if 2 <= panose.serif_style <= 10:
            return "serif"
        if 11 <= panose.serif_style <= 15:
            return "sans"
    return "sans"


__all__ = [
    "EmbeddingPermissions",
    "FontError",
    "FontIntent",
    "FontMatch",
    "FontMetadata",
    "FontNameRecord",
    "FontRef",
    "FontTraits",
    "FontTranslationReport",
    "FontUnsupportedError",
    "Panose",
    "ScoreBreakdown",
    "TargetFontProfile",
    "TypographyRef",
    "VariableAxis",
    "candidate_supports_target",
    "canonical_feature_name",
    "choose_font",
    "codepoints_from_glyphs",
    "feature_coverage",
    "formats_compatible",
    "is_bitmap_format",
    "license_is_allowlisted",
    "match_font",
    "match_fonts",
    "metric_score",
    "missing_features",
    "missing_scripts",
    "normalize_feature",
    "normalize_format",
    "normalize_script",
    "panose_distance",
    "parse_range",
    "plan_export",
    "role_score",
    "score_candidate",
    "script_coverage",
    "trait_shape_score",
]
