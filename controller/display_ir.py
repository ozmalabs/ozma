# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Controller-side lossless Widget IR contracts.

This module is the Python counterpart of the Rust ``ozma-display-ir`` crate.
It validates the same JSON shapes as ``schemas/display/*.schema.json`` and
provides the first ``ScreenLayout`` / ``DisplayDefinition`` mappings used by
controller importers.

The typed shapes below are :class:`typing.TypedDict` equivalents of the Rust
serde models. Validation accepts mappings (and the typed dicts, which are
mappings at runtime) so callers do not have to construct model objects.

Imports are lossless. ``SourceCaptureReport["lossless"]`` must be ``True``.
Exports must carry a ``TranslationReport`` with ``direction == "export"``.
Unknown screen-layout fields are retained under ``extensions`` so that
``from_screen_layout`` -> ``to_screen_layout`` is not destructive.
"""

from __future__ import annotations

import json
import math
import re
from typing import Any, Mapping, NotRequired, Required, TypedDict

# ---------------------------------------------------------------------------
# Vocabularies
# ---------------------------------------------------------------------------

IR_SCHEMA = "org.ozma.widget-ir/1"
VIBE_TAXONOMY = "org.ozma.vibe/1"
SOURCE_FORMAT_OZMA = "ozma_display_definition"

WIDGET_KINDS = frozenset(
    {
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
    }
)
ASSET_KINDS = frozenset(
    {"original", "image", "font", "script", "theme", "preview", "other"}
)
SANDBOX_MODES = frozenset({"iframe", "wasm", "none"})
PACK_TYPES = frozenset(
    {"widget_pack", "theme_pack", "description_pack", "import_pack"}
)
PACK_CONFIDENCE = frozenset({"exact", "approximate", "community", "template"})
PROVENANCE_KINDS = frozenset({"original", "imported", "adapted", "community"})
SYNC_MODES = frozenset({"full", "installed", "manual", "offline"})
FIDELITIES = frozenset({"exact", "semantic", "approximate", "visual_only"})

_SHA256_RE = re.compile(r"^[0-9a-fA-F]{64}$")
_SAFE_INTEGER = 9_007_199_254_740_992


class DisplayIrError(ValueError):
    """A display IR document is invalid."""


# ---------------------------------------------------------------------------
# JSON shape equivalents
# ---------------------------------------------------------------------------


class DataBinding(TypedDict, total=False):
    metric: Required[str]
    source: str
    unit: str
    scale: float
    format: str
    field_id: int
    value_type: str
    property: str


class Threshold(TypedDict, total=False):
    at: Required[float]
    severity: Required[str]
    label: str
    color: str


class SeverityBand(TypedDict, total=False):
    severity: Required[str]
    min: float
    max: float
    label: str


class ValueDomain(TypedDict, total=False):
    min: float
    max: float
    nominal: float
    thresholds: list[Threshold]
    severity_bands: list[SeverityBand]


class HistoryPolicy(TypedDict, total=False):
    window_seconds: float
    sample_rate_hz: float
    aggregation: str
    retention_points: int


class IconRef(TypedDict, total=False):
    name: str
    asset_id: str
    set: str


class ActionRef(TypedDict, total=False):
    key: Required[str]
    action_id: int
    label: str
    params: Any
    permissions: list[str]


class Accessibility(TypedDict, total=False):
    label: str
    description: str
    role: str
    keyboard_shortcut: str
    reduced_motion: bool


class LayoutHint(TypedDict, total=False):
    x: float
    y: float
    width: float
    height: float
    zone: str
    responsive_priority: int
    aspect_ratio: float


class StyleRef(TypedDict, total=False):
    theme: str
    tokens: dict[str, Any]
    classes: list[str]


class TypographyRef(TypedDict, total=False):
    intent: str
    size_px: float
    size_pt: float
    line_height: float
    tracking: float
    weight: int
    features: list[str]


class AssetRef(TypedDict, total=False):
    id: Required[str]
    path: Required[str]
    mime: Required[str]
    sha256: Required[str]
    size: Required[int]
    kind: Required[str]
    executable: Required[bool]
    sandbox: str


class FontAssetRef(TypedDict, total=False):
    id: Required[str]
    path: Required[str]
    mime: Required[str]
    sha256: Required[str]
    size: Required[int]
    kind: Required[str]
    executable: Required[bool]
    sandbox: str
    family: str
    style: str
    format: str
    license: str
    embedding_allowed: bool
    private: bool


class SourceDocument(TypedDict, total=False):
    format: Required[str]
    source_hash: Required[str]
    version: str
    original: AssetRef
    parsed: Any
    unknown: Any


class SourceCaptureReport(TypedDict, total=False):
    source_format: Required[str]
    source_hash: Required[str]
    lossless: Required[bool]
    source_version: str
    captured: list[str]
    opaque: list[str]
    assets: list[str]


class TranslationReport(TypedDict, total=False):
    source_format: Required[str]
    target_format: Required[str]
    translator: Required[str]
    translator_version: Required[str]
    fidelity: Required[str]
    direction: NotRequired[str]
    converted: list[str]
    approximated: list[str]
    dropped: list[str]
    warnings: list[str]


class VibeProvenance(TypedDict, total=False):
    source_keywords: list[str]
    derived_keywords: list[str]
    user_keywords: list[str]
    methods: list[str]


class VibeProfile(TypedDict, total=False):
    taxonomy: str
    keywords: list[str]
    mood: list[str]
    era: str
    form: list[str]
    material: list[str]
    density: str
    color: list[str]
    typography: list[str]
    motion: list[str]
    references: list[str]
    ornament: list[str]
    confidence: float
    provenance: VibeProvenance


class ShapeSignature(TypedDict, total=False):
    widget_kind: str
    aspect_ratio: float
    corner_radius_ratio: float
    stroke_width_ratio: float
    fill_ratio: float
    symmetry: str
    arc_start_deg: float
    arc_end_deg: float
    tick_count: int
    segment_count: int
    needle_style: str
    chart_mark: str
    texture: str
    elevation: str
    glow: bool
    animation: str


class CoherenceDimensions(TypedDict, total=False):
    mood: float
    era: float
    form: float
    material: float
    color: float
    typography: float
    density: float
    motion: float


class CoherenceReport(TypedDict, total=False):
    score: Required[float]
    confidence: Required[float]
    dimensions: CoherenceDimensions
    outliers: list[str]
    conflicts: list[str]
    suggestions: list[str]


class Provenance(TypedDict, total=False):
    type: Required[str]
    source: str
    author: str
    method: str
    notes: str


class AssetPackEntry(TypedDict, total=False):
    id: Required[str]
    type: Required[str]
    name: Required[str]
    version: Required[str]
    schema: Required[str]
    license: Required[str]
    author: str
    description: str
    source_url: str
    tags: list[str]
    capabilities: list[str]
    permissions: list[str]
    min_ozma_version: str
    min_display_schema: str
    assets: list[AssetRef]
    preview: str
    provenance: Provenance
    confidence: str
    vibe: VibeProfile
    shape: ShapeSignature
    coherence: CoherenceReport
    signature: str
    published_at: str
    yanked: bool
    source_capture: SourceCaptureReport
    translation_reports: list[TranslationReport]


class AssetSyncPolicy(TypedDict, total=False):
    mode: Required[str]
    sync_interval_hours: int
    cache_size_mb: int
    include_previews: bool
    include_external_imports: bool
    verify_signatures: bool


class WidgetIR(TypedDict, total=False):
    id: Required[str]
    kind: Required[str]
    binding: Required[DataBinding]
    source: Required[SourceDocument]
    role: str
    domain: ValueDomain
    history: HistoryPolicy
    label: str
    icon: IconRef
    actions: list[ActionRef]
    accessibility: Accessibility
    layout: LayoutHint
    style: StyleRef
    typography: TypographyRef
    fonts: list[FontAssetRef]
    vibe: VibeProfile
    shape: ShapeSignature
    extensions: dict[str, Any]
    assets: list[AssetRef]


# ---------------------------------------------------------------------------
# Primitive validation helpers
# ---------------------------------------------------------------------------


def _mapping(value: Any, field: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise DisplayIrError(f"`{field}` must be an object")
    return value


def _require_non_empty(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise DisplayIrError(f"`{field}` must not be empty")
    return value


def _require_finite(value: Any, field: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise DisplayIrError(f"`{field}` must be a number")
    number = float(value)
    if not math.isfinite(number):
        raise DisplayIrError(f"`{field}` must be finite")
    return number


def _require_unit_interval(value: Any, field: str) -> float:
    number = _require_finite(value, field)
    if not 0.0 <= number <= 1.0:
        raise DisplayIrError(f"`{field}` must be between 0 and 1, got {number}")
    return number


def _require_non_negative(value: Any, field: str) -> float:
    number = _require_finite(value, field)
    if number < 0.0:
        raise DisplayIrError(f"`{field}` must be non-negative, got {number}")
    return number


def _validate_sha256(value: Any, field: str) -> str:
    if not isinstance(value, str) or not _SHA256_RE.match(value):
        raise DisplayIrError(
            f"`{field}` must be a 64-character hexadecimal SHA-256 digest"
        )
    return value


def _string_array(value: Any, field: str) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, list) or any(not isinstance(item, str) for item in value):
        raise DisplayIrError(f"`{field}` must be an array of strings")
    return value


# ---------------------------------------------------------------------------
# Domain validation
# ---------------------------------------------------------------------------


def _validate_binding(binding: Mapping[str, Any]) -> None:
    _require_non_empty(binding.get("metric"), "binding.metric")
    if binding.get("scale") is not None:
        _require_finite(binding["scale"], "binding.scale")
    value_type = binding.get("value_type")
    if value_type is not None and not isinstance(value_type, str):
        raise DisplayIrError("`binding.value_type` must be a string")


def _validate_domain(domain: Mapping[str, Any]) -> None:
    for key in ("min", "max", "nominal"):
        if domain.get(key) is not None:
            _require_finite(domain[key], f"domain.{key}")
    if domain.get("min") is not None and domain.get("max") is not None:
        if float(domain["min"]) > float(domain["max"]):
            raise DisplayIrError("`domain.min` must be less than or equal to `domain.max`")
    thresholds = domain.get("thresholds") or []
    if not isinstance(thresholds, list):
        raise DisplayIrError("`domain.thresholds` must be an array")
    for item in thresholds:
        threshold = _mapping(item, "domain.thresholds[]")
        _require_finite(threshold.get("at"), "domain.thresholds[].at")
        _require_non_empty(threshold.get("severity"), "domain.thresholds[].severity")
    bands = domain.get("severity_bands") or []
    if not isinstance(bands, list):
        raise DisplayIrError("`domain.severity_bands` must be an array")
    for item in bands:
        band = _mapping(item, "domain.severity_bands[]")
        for key in ("min", "max"):
            if band.get(key) is not None:
                _require_finite(band[key], f"domain.severity_bands[].{key}")
        if band.get("min") is not None and band.get("max") is not None:
            if float(band["min"]) > float(band["max"]):
                raise DisplayIrError(
                    "`domain.severity_bands[].min` must be less than or equal to max"
                )
        _require_non_empty(
            band.get("severity"), "domain.severity_bands[].severity"
        )


def _validate_history(history: Mapping[str, Any]) -> None:
    if history.get("window_seconds") is not None:
        _require_non_negative(history["window_seconds"], "history.window_seconds")
    if history.get("sample_rate_hz") is not None:
        rate = _require_finite(history["sample_rate_hz"], "history.sample_rate_hz")
        if rate <= 0.0:
            raise DisplayIrError("`history.sample_rate_hz` must be greater than zero")


def _validate_layout(layout: Mapping[str, Any]) -> None:
    for key in ("x", "y", "width", "height"):
        if layout.get(key) is not None:
            _require_finite(layout[key], f"layout.{key}")
    for key in ("width", "height"):
        if layout.get(key) is not None and float(layout[key]) <= 0.0:
            raise DisplayIrError(f"`layout.{key}` must be greater than zero")
    if layout.get("aspect_ratio") is not None:
        ratio = _require_finite(layout["aspect_ratio"], "layout.aspect_ratio")
        if ratio <= 0.0:
            raise DisplayIrError("`layout.aspect_ratio` must be greater than zero")


def _validate_typography(typography: Mapping[str, Any]) -> None:
    for key in ("size_px", "size_pt"):
        if typography.get(key) is not None and float(typography[key]) <= 0.0:
            raise DisplayIrError(f"`typography.{key}` must be greater than zero")
    for key in ("line_height", "tracking", "size_px", "size_pt"):
        if typography.get(key) is not None:
            _require_finite(typography[key], f"typography.{key}")
    weight = typography.get("weight")
    if weight is not None and not 1 <= int(weight) <= 1000:
        raise DisplayIrError("`typography.weight` must be between 1 and 1000")


def validate_asset(asset: Mapping[str, Any]) -> None:
    """Validate one :class:`AssetRef`."""
    asset = _mapping(asset, "asset")
    _require_non_empty(asset.get("id"), "assets[].id")
    _require_non_empty(asset.get("path"), "assets[].path")
    _require_non_empty(asset.get("mime"), "assets[].mime")
    _validate_sha256(asset.get("sha256"), "assets[].sha256")
    kind = asset.get("kind")
    if kind not in ASSET_KINDS:
        raise DisplayIrError(f"unknown asset kind `{kind}`")
    if asset.get("size") is not None:
        size = asset["size"]
        if isinstance(size, bool) or not isinstance(size, int) or size < 0:
            raise DisplayIrError("`assets[].size` must be a non-negative integer")
    sandbox = asset.get("sandbox")
    if sandbox is not None and sandbox not in SANDBOX_MODES:
        raise DisplayIrError(f"unknown sandbox mode `{sandbox}`")
    if asset.get("executable"):
        if sandbox not in {"iframe", "wasm"}:
            raise DisplayIrError(
                "executable assets require an `iframe` or `wasm` sandbox"
            )


def validate_source(source: Mapping[str, Any]) -> None:
    """Validate a :class:`SourceDocument`."""
    source = _mapping(source, "source")
    _require_non_empty(source.get("format"), "source.format")
    _validate_sha256(source.get("source_hash"), "source.source_hash")
    original = source.get("original")
    if original is not None:
        validate_asset(original)


def validate_vibe(profile: Mapping[str, Any]) -> None:
    """Validate a :class:`VibeProfile`; unknown keywords are kept, not rejected."""
    profile = _mapping(profile, "vibe")
    taxonomy = profile.get("taxonomy", VIBE_TAXONOMY)
    _require_non_empty(taxonomy, "vibe.taxonomy")
    if profile.get("confidence") is not None:
        _require_unit_interval(profile["confidence"], "vibe.confidence")


def validate_shape(shape: Mapping[str, Any]) -> None:
    """Validate a :class:`ShapeSignature`."""
    shape = _mapping(shape, "shape")
    for key in (
        "aspect_ratio",
        "corner_radius_ratio",
        "stroke_width_ratio",
        "fill_ratio",
    ):
        if shape.get(key) is not None:
            _require_non_negative(shape[key], f"shape.{key}")
    for key in ("arc_start_deg", "arc_end_deg"):
        if shape.get(key) is not None:
            _require_finite(shape[key], f"shape.{key}")


def validate_coherence(report: Mapping[str, Any]) -> None:
    """Validate a :class:`CoherenceReport` including every dimension."""
    report = _mapping(report, "coherence")
    _require_unit_interval(report.get("score"), "coherence.score")
    if report.get("confidence") is not None:
        _require_unit_interval(report["confidence"], "coherence.confidence")
    dimensions = report.get("dimensions") or {}
    dimensions = _mapping(dimensions, "coherence.dimensions")
    for key, value in dimensions.items():
        _require_unit_interval(value, f"coherence.dimensions.{key}")


def validate_source_capture(report: Mapping[str, Any]) -> Mapping[str, Any]:
    """Validate an import capture report; lossy imports are rejected."""
    report = _mapping(report, "source_capture")
    _require_non_empty(report.get("source_format"), "source_capture.source_format")
    _validate_sha256(report.get("source_hash"), "source_capture.source_hash")
    if report.get("lossless") is not True:
        raise DisplayIrError(
            "`source_capture.lossless` must be true; imports must be lossless"
        )
    for key in ("captured", "opaque", "assets"):
        _string_array(report.get(key), f"source_capture.{key}")
    return report


def validate_translation_report(report: Mapping[str, Any]) -> Mapping[str, Any]:
    """Validate an export report."""
    report = _mapping(report, "translation_report")
    if report.get("direction") != "export":
        raise DisplayIrError("translation report direction must be `export`")
    for key in ("source_format", "target_format", "translator", "translator_version"):
        _require_non_empty(report.get(key), f"translation_report.{key}")
    fidelity = report.get("fidelity")
    if fidelity not in FIDELITIES:
        raise DisplayIrError(f"unknown translation fidelity `{fidelity}`")
    for key in ("converted", "approximated", "dropped", "warnings"):
        _string_array(report.get(key), f"translation_report.{key}")
    return report


def require_export_report(
    report: Mapping[str, Any] | None, target: str
) -> Mapping[str, Any]:
    """Require a valid export report for ``target``."""
    if report is None:
        raise DisplayIrError(f"export to `{target}` requires a TranslationReport")
    validate_translation_report(report)
    return report


def validate_ir(ir: Mapping[str, Any]) -> None:
    """Validate a :class:`WidgetIR` with the same rules as the Rust crate."""
    ir = _mapping(ir, "ir")
    _require_non_empty(ir.get("id"), "id")
    kind = ir.get("kind")
    if kind not in WIDGET_KINDS:
        raise DisplayIrError(f"unknown widget kind `{kind}`")
    binding = _mapping(ir.get("binding"), "binding")
    _validate_binding(binding)
    validate_source(_mapping(ir.get("source"), "source"))
    if ir.get("domain") is not None:
        _validate_domain(_mapping(ir["domain"], "domain"))
    if ir.get("history") is not None:
        _validate_history(_mapping(ir["history"], "history"))
    if ir.get("layout") is not None:
        _validate_layout(_mapping(ir["layout"], "layout"))
    if ir.get("typography") is not None:
        _validate_typography(_mapping(ir["typography"], "typography"))
    if ir.get("vibe") is not None:
        validate_vibe(_mapping(ir["vibe"], "vibe"))
    if ir.get("shape") is not None:
        validate_shape(_mapping(ir["shape"], "shape"))
    actions = ir.get("actions") or []
    if not isinstance(actions, list):
        raise DisplayIrError("`actions` must be an array")
    for item in actions:
        _mapping(item, "actions[]")
        _require_non_empty(item.get("key"), "actions[].key")
    _validate_asset_ids(ir)


def _validate_asset_ids(ir: Mapping[str, Any]) -> None:
    seen: set[str] = set()
    assets = ir.get("assets") or []
    fonts = ir.get("fonts") or []
    if not isinstance(assets, list) or not isinstance(fonts, list):
        raise DisplayIrError("`assets` and `fonts` must be arrays")
    for asset in list(assets) + list(fonts):
        asset = _mapping(asset, "asset")
        validate_asset(asset)
        asset_id = asset.get("id")
        if asset_id in seen:
            raise DisplayIrError(f"duplicate asset id `{asset_id}`")
        seen.add(str(asset_id))


def validate_provenance(provenance: Mapping[str, Any]) -> None:
    """Validate a provenance record."""
    provenance = _mapping(provenance, "provenance")
    kind = provenance.get("type")
    if kind not in PROVENANCE_KINDS:
        raise DisplayIrError(f"unknown provenance type `{kind}`")


def validate_asset_pack(entry: Mapping[str, Any]) -> None:
    """Validate an :class:`AssetPackEntry`, including import/export reports."""
    entry = _mapping(entry, "asset_pack")
    _require_non_empty(entry.get("id"), "id")
    pack_type = entry.get("type")
    if pack_type not in PACK_TYPES:
        raise DisplayIrError(f"unknown asset pack type `{pack_type}`")
    for key in ("name", "version", "schema", "license"):
        _require_non_empty(entry.get(key), key)
    confidence = entry.get("confidence")
    # Legacy packs omit confidence; empty is treated as unknown, not invalid.
    if confidence and confidence not in PACK_CONFIDENCE:
        raise DisplayIrError(f"unknown pack confidence `{confidence}`")
    if entry.get("provenance"):
        validate_provenance(entry["provenance"])
    assets = entry.get("assets") or []
    if not isinstance(assets, list):
        raise DisplayIrError("`assets` must be an array")
    seen: set[str] = set()
    for asset in assets:
        validate_asset(asset)
        asset_id = asset.get("id")
        if asset_id in seen:
            raise DisplayIrError(f"duplicate asset id `{asset_id}`")
        seen.add(str(asset_id))
    # Legacy packs may emit empty optional design-language blocks. An empty
    # mapping carries no claims, so treat it as absent rather than invalid.
    if entry.get("vibe"):
        validate_vibe(entry["vibe"])
    if entry.get("shape"):
        validate_shape(entry["shape"])
    if entry.get("coherence"):
        validate_coherence(entry["coherence"])
    if entry.get("source_capture") is not None:
        validate_source_capture(entry["source_capture"])
        if not entry.get("translation_reports"):
            raise DisplayIrError(
                "`translation_reports` must contain an export report for an imported pack"
            )
    reports = entry.get("translation_reports") or []
    if not isinstance(reports, list):
        raise DisplayIrError("`translation_reports` must be an array")
    for report in reports:
        validate_translation_report(report)


def validate_asset_pack_for_export(
    entry: Mapping[str, Any], target_format: str
) -> Mapping[str, Any]:
    """Validate an entry and require a report for ``target_format``."""
    validate_asset_pack(entry)
    for report in entry.get("translation_reports") or []:
        if report.get("target_format") == target_format:
            return require_export_report(report, target_format)
    raise DisplayIrError(f"export to `{target_format}` requires a TranslationReport")


def validate_sync_policy(policy: Mapping[str, Any]) -> None:
    """Validate an :class:`AssetSyncPolicy`."""
    policy = _mapping(policy, "sync_policy")
    mode = policy.get("mode")
    if mode not in SYNC_MODES:
        raise DisplayIrError(f"unknown asset sync mode `{mode}`")
    interval = policy.get("sync_interval_hours", 24)
    if not isinstance(interval, int) or isinstance(interval, bool) or interval <= 0:
        raise DisplayIrError("`sync_interval_hours` must be greater than zero")
    cache_size = policy.get("cache_size_mb")
    if cache_size is not None and (
        not isinstance(cache_size, int) or isinstance(cache_size, bool) or cache_size <= 0
    ):
        raise DisplayIrError("`cache_size_mb` must be greater than zero when present")


# Short aliases for importers that name the document rather than the action.
validate_widget_ir = validate_ir
validate_capture = validate_source_capture
validate_report = validate_translation_report


# ---------------------------------------------------------------------------
# Canonical JSON and hashing
# ---------------------------------------------------------------------------


def _canonical_string(text: str) -> str:
    return json.dumps(text, ensure_ascii=False, separators=(",", ":"))


def _canonical_number(value: int | float) -> str:
    if isinstance(value, int):
        return str(value)
    if not math.isfinite(value):
        raise DisplayIrError("canonical JSON cannot encode NaN or infinity")
    if value == 0.0:
        return "0"
    if value.is_integer() and abs(value) <= _SAFE_INTEGER:
        return str(int(value))
    text = repr(value)
    if "e" in text or "E" in text:
        mantissa, exponent = re.split("[eE]", text, maxsplit=1)
        sign = ""
        if exponent[:1] in {"+", "-"}:
            sign, exponent = exponent[0], exponent[1:]
        exponent = exponent.lstrip("0") or "0"
        text = f"{mantissa}e{sign if sign == '-' else ''}{exponent}"
    if text.endswith(".0"):
        text = text[:-2]
    return text


def _write_canonical(value: Any, out: list[str]) -> None:
    if value is None:
        out.append("null")
    elif value is True:
        out.append("true")
    elif value is False:
        out.append("false")
    elif isinstance(value, int):
        out.append(str(value))
    elif isinstance(value, float):
        out.append(_canonical_number(value))
    elif isinstance(value, str):
        out.append(_canonical_string(value))
    elif isinstance(value, (list, tuple)):
        out.append("[")
        for index, item in enumerate(value):
            if index:
                out.append(",")
            _write_canonical(item, out)
        out.append("]")
    elif isinstance(value, Mapping):
        out.append("{")
        keys = list(value.keys())
        if any(not isinstance(key, str) for key in keys):
            raise DisplayIrError("canonical JSON object keys must be strings")
        for index, key in enumerate(sorted(keys)):
            if index:
                out.append(",")
            out.append(_canonical_string(key))
            out.append(":")
            _write_canonical(value[key], out)
        out.append("}")
    else:
        raise DisplayIrError(
            f"canonical JSON cannot encode {type(value).__name__}"
        )


def canonical_json(value: Any) -> str:
    """Deterministic, compact JSON with recursively sorted object keys."""
    out: list[str] = []
    _write_canonical(value, out)
    return "".join(out)


def canonical_hash(value: Any) -> str:
    """BLAKE3 hash of :func:`canonical_json`, as lowercase hexadecimal."""
    try:
        import blake3
    except ImportError as exc:  # pragma: no cover - requirements install it
        raise DisplayIrError(
            "blake3 is required for canonical hashing; install blake3"
        ) from exc
    return blake3.blake3(canonical_json(value).encode("utf-8")).hexdigest()


# ---------------------------------------------------------------------------
# ScreenLayout / DisplayDefinition mapping
# ---------------------------------------------------------------------------

_SCREEN_TYPES = {"text": "label", "chart": "sparkline", "status": "icon"}
_REVERSE_SCREEN_TYPES = {value: key for key, value in _SCREEN_TYPES.items()}

_RESERVED_LAYOUT_KEYS = {"id", "width", "height", "widgets", "fields"}

_GENERATED_WIDGET_KEYS = {
    "type",
    "ir_kind",
    "ir_id",
    "x",
    "y",
    "w",
    "h",
    "metric",
    "field",
    "unit",
    "label",
    "format",
    "min",
    "max",
    "warn_color",
    "crit_color",
    "font_size",
    "icon",
    "icon_asset",
    "accessibility_label",
    "action",
    "action_id",
    "history_seconds",
}


def _as_mapping(value: Any) -> Mapping[str, Any]:
    return _mapping(value, "ir")


def _value_type(ir: Mapping[str, Any]) -> str:
    binding = _mapping(ir.get("binding", {}), "binding")
    declared = binding.get("value_type")
    if isinstance(declared, str) and declared:
        return declared.lower()
    return "string" if ir.get("kind") == "text" else "real"


def _field_id(ir: Mapping[str, Any]) -> int:
    field_id = _mapping(ir.get("binding", {}), "binding").get("field_id")
    if isinstance(field_id, int) and not isinstance(field_id, bool) and field_id > 0:
        return field_id
    return 1


def _scale_value(ir: Mapping[str, Any]) -> int | None:
    binding = _mapping(ir.get("binding", {}), "binding")
    scale = binding.get("scale")
    if isinstance(scale, bool) or not isinstance(scale, (int, float)):
        return None
    number = float(scale)
    if not math.isfinite(number) or not number.is_integer():
        return None
    return int(number)


def _compiled_fields(widgets: list[Mapping[str, Any]]) -> list[dict[str, Any]]:
    bindings: dict[str, Mapping[str, Any]] = {}
    for widget in widgets:
        metric = _mapping(widget.get("binding", {}), "binding").get("metric")
        if isinstance(metric, str) and metric and metric not in bindings:
            bindings[metric] = widget
    used: set[int] = set()
    ids: dict[str, int] = {}
    for key in sorted(bindings):
        widget = bindings[key]
        explicit = _mapping(widget.get("binding", {}), "binding").get("field_id")
        if (
            isinstance(explicit, int)
            and not isinstance(explicit, bool)
            and explicit > 0
            and explicit not in used
        ):
            ids[key] = explicit
            used.add(explicit)
    next_id = 1
    for key in sorted(bindings):
        if key in ids:
            continue
        while next_id in used:
            next_id += 1
        ids[key] = next_id
        used.add(next_id)
        next_id += 1

    compiled: list[dict[str, Any]] = []
    for key in sorted(bindings):
        widget = bindings[key]
        binding = _mapping(widget.get("binding", {}), "binding")
        entry: dict[str, Any] = {
            "id": ids[key],
            "key": key,
            "type": _value_type(widget),
        }
        if binding.get("unit"):
            entry["unit"] = str(binding["unit"])
        scale = _scale_value(widget)
        if scale is not None:
            entry["scale"] = scale
        compiled.append(entry)
    return compiled


def _compiled_actions(widgets: list[Mapping[str, Any]]) -> list[dict[str, Any]]:
    actions: dict[str, Mapping[str, Any]] = {}
    for widget in widgets:
        for action in widget.get("actions") or []:
            if not isinstance(action, Mapping):
                continue
            key = action.get("key")
            if isinstance(key, str) and key and key not in actions:
                actions[key] = action
    used: set[int] = set()
    ids: dict[str, int] = {}
    for key in sorted(actions):
        explicit = actions[key].get("action_id")
        if (
            isinstance(explicit, int)
            and not isinstance(explicit, bool)
            and explicit > 0
            and explicit not in used
        ):
            ids[key] = explicit
            used.add(explicit)
    next_id = 1
    for key in sorted(actions):
        if key in ids:
            continue
        while next_id in used:
            next_id += 1
        ids[key] = next_id
        used.add(next_id)
        next_id += 1

    compiled: list[dict[str, Any]] = []
    for key in sorted(actions):
        action = actions[key]
        entry: dict[str, Any] = {
            "id": ids[key],
            "key": key,
            "params_schema": {},
        }
        if action.get("label") is not None:
            entry["label"] = str(action["label"])
        permissions = action.get("permissions")
        if permissions:
            entry["permissions"] = [str(item) for item in permissions]
        compiled.append(entry)
    return compiled


def _screen_widget(
    ir: Mapping[str, Any], field_id: int, action_id: int | None
) -> tuple[dict[str, Any], float, float]:
    layout = _mapping(ir.get("layout") or {}, "layout")
    x = _round_number(layout.get("x", 0.0))
    y = _round_number(layout.get("y", 0.0))
    width = max(1, _round_number(layout.get("width", 120.0)))
    height = max(1, _round_number(layout.get("height", 120.0)))
    kind = ir.get("kind")
    binding = _mapping(ir.get("binding") or {}, "binding")

    widget: dict[str, Any] = {
        "type": _SCREEN_TYPES.get(kind, kind),
        "ir_kind": kind,
        "ir_id": ir.get("id"),
        "x": x,
        "y": y,
        "w": width,
        "h": height,
        "metric": binding.get("metric", ""),
        "field": field_id,
    }
    if binding.get("unit"):
        widget["unit"] = str(binding["unit"])
    if binding.get("format"):
        widget["format"] = str(binding["format"])
    if ir.get("label") is not None:
        widget["label"] = str(ir["label"])

    domain = ir.get("domain")
    if isinstance(domain, Mapping):
        if domain.get("min") is not None:
            widget["min"] = domain["min"]
        if domain.get("max") is not None:
            widget["max"] = domain["max"]
        for threshold in domain.get("thresholds") or []:
            if not isinstance(threshold, Mapping):
                continue
            severity = str(threshold.get("severity", "")).lower()
            color = threshold.get("color")
            if not color:
                continue
            if severity in {"warning", "warn"}:
                widget["warn_color"] = str(color)
            elif severity in {"critical", "crit", "error"}:
                widget["crit_color"] = str(color)

    typography = ir.get("typography")
    if isinstance(typography, Mapping) and typography.get("size_px") is not None:
        widget["font_size"] = _round_number(typography["size_px"])

    icon = ir.get("icon")
    if isinstance(icon, Mapping):
        if icon.get("name"):
            widget["icon"] = str(icon["name"])
        elif icon.get("asset_id"):
            widget["icon_asset"] = str(icon["asset_id"])

    accessibility = ir.get("accessibility")
    if isinstance(accessibility, Mapping) and accessibility.get("label"):
        widget["accessibility_label"] = str(accessibility["label"])

    actions = ir.get("actions") or []
    if actions and isinstance(actions[0], Mapping):
        action = actions[0]
        if action.get("key"):
            widget["action"] = str(action["key"])
            if action_id is not None:
                widget["action_id"] = action_id

    history = ir.get("history")
    if isinstance(history, Mapping) and history.get("window_seconds") is not None:
        seconds = float(history["window_seconds"])
        if seconds.is_integer():
            widget["history_seconds"] = int(seconds)

    extensions = ir.get("extensions")
    if isinstance(extensions, Mapping):
        reserved = extensions.get("screen_layout")
        if isinstance(reserved, Mapping) and isinstance(
            reserved.get("widget"), Mapping
        ):
            widget.update(reserved["widget"])
    return widget, x + width, y + height


def _round_number(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise DisplayIrError(f"expected a number, got {value!r}")
    number = float(value)
    if not math.isfinite(number):
        raise DisplayIrError("layout coordinates must be finite")
    return int(round(number))


def to_screen_layout(ir: Mapping[str, Any]) -> dict[str, Any]:
    """Map one :class:`WidgetIR` to a ``ScreenLayout``-compatible document."""
    ir = _as_mapping(ir)
    widgets = [ir]
    fields = _compiled_fields(widgets)
    field_id = fields[0]["id"] if fields else _field_id(ir)
    actions = _compiled_actions(widgets)
    action_id = actions[0]["id"] if actions else None
    widget, right, bottom = _screen_widget(ir, field_id, action_id)

    layout: dict[str, Any] = {
        "id": ir.get("id", "widget"),
        "name": ir.get("id", "widget"),
        "width": int(round(max(480.0, right))),
        "height": int(round(max(480.0, bottom))),
        "widgets": [widget],
    }
    if fields:
        layout["fields"] = fields
    extensions = ir.get("extensions")
    if isinstance(extensions, Mapping):
        reserved = extensions.get("screen_layout")
        if isinstance(reserved, Mapping) and isinstance(
            reserved.get("layout"), Mapping
        ):
            for key, value in reserved["layout"].items():
                if key not in _RESERVED_LAYOUT_KEYS:
                    layout[key] = value
    return layout


def widget_irs_to_definitions(
    widgets: list[Mapping[str, Any]],
    definition_id: str | None = None,
    version: int = 1,
) -> dict[str, Any]:
    """Map several widgets to a definition document.

    The returned object has the canonical ``id``, ``version``, ``layout``,
    ``fields``, and ``actions`` members plus ``meta.ozma_ir``.
    """
    if not widgets:
        raise DisplayIrError("at least one widget is required")
    widgets = [_as_mapping(widget) for widget in widgets]
    if definition_id is None:
        definition_id = str(widgets[0].get("id", "widget"))
    fields = _compiled_fields(widgets)
    actions = _compiled_actions(widgets)
    field_ids = {entry["key"]: entry["id"] for entry in fields}
    action_ids = {entry["key"]: entry["id"] for entry in actions}

    widget_docs: list[dict[str, Any]] = []
    width = 480.0
    height = 480.0
    for widget in widgets:
        binding = _mapping(widget.get("binding") or {}, "binding")
        metric = binding.get("metric")
        field_id = field_ids.get(metric, _field_id(widget))
        action_id = None
        action_list = widget.get("actions") or []
        if action_list and isinstance(action_list[0], Mapping):
            action_id = action_ids.get(action_list[0].get("key"))
        doc, right, bottom = _screen_widget(widget, field_id, action_id)
        width = max(width, right)
        height = max(height, bottom)
        widget_docs.append(doc)

    layout: dict[str, Any] = {
        "id": definition_id,
        "name": definition_id,
        "width": int(round(width)),
        "height": int(round(height)),
        "widgets": widget_docs,
    }
    if fields:
        layout["fields"] = fields
    extensions = widgets[0].get("extensions")
    if isinstance(extensions, Mapping):
        reserved = extensions.get("screen_layout")
        if isinstance(reserved, Mapping) and isinstance(
            reserved.get("layout"), Mapping
        ):
            for key, value in reserved["layout"].items():
                if key not in _RESERVED_LAYOUT_KEYS:
                    layout[key] = value

    return {
        "id": definition_id,
        "version": int(version),
        "layout": layout,
        "fields": fields,
        "actions": actions,
        "meta": {"ozma_ir": compact_projection(widgets)},
    }


def to_definition_json(
    ir: Mapping[str, Any],
    definition_id: str | None = None,
    version: int = 1,
) -> dict[str, Any]:
    """Map one :class:`WidgetIR` to a ``DisplayDefinition`` JSON object."""
    return widget_irs_to_definitions([ir], definition_id=definition_id, version=version)


def compact_projection(widgets: list[Mapping[str, Any]]) -> dict[str, Any]:
    """Compact IR projection stored at ``definition.meta.ozma_ir``."""
    projections: list[dict[str, Any]] = []
    for widget in widgets:
        widget = _as_mapping(widget)
        projection: dict[str, Any] = {
            "id": widget.get("id"),
            "kind": widget.get("kind"),
        }
        if widget.get("role") is not None:
            projection["role"] = widget["role"]
        binding = _mapping(widget.get("binding") or {}, "binding")
        projection["metric"] = binding.get("metric")
        if binding.get("unit") is not None:
            projection["unit"] = binding["unit"]
        if widget.get("label") is not None:
            projection["label"] = widget["label"]
        source = _mapping(widget.get("source") or {}, "source")
        projection["source_format"] = source.get("format")
        projection["source_hash"] = source.get("source_hash")
        vibe = widget.get("vibe")
        if isinstance(vibe, Mapping):
            projection["vibe"] = {
                "keywords": list(vibe.get("keywords") or []),
                "confidence": vibe.get("confidence", 1.0),
            }
        if widget.get("shape") is not None:
            projection["shape"] = widget["shape"]
        asset_ids = [
            asset.get("id")
            for asset in (widget.get("assets") or [])
            if isinstance(asset, Mapping)
        ]
        asset_ids.extend(
            font.get("id")
            for font in (widget.get("fonts") or [])
            if isinstance(font, Mapping)
        )
        if asset_ids:
            projection["asset_ids"] = asset_ids
        projections.append(projection)
    return {"schema": IR_SCHEMA, "widgets": projections, "lossless": True}


def from_screen_layout(
    layout: Mapping[str, Any],
    source: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Map a ``ScreenLayout`` document back to one :class:`WidgetIR` dict.

    Fields that this typed projection does not model are preserved under
    ``extensions.screen_layout`` (``layout`` for top-level unknowns and
    ``widget`` for widget-level unknowns), so a later
    :func:`to_screen_layout` restores them.
    """
    layout = _mapping(layout, "layout")
    widgets = layout.get("widgets")
    if not isinstance(widgets, list) or not widgets:
        raise DisplayIrError("screen layout must contain at least one widget")
    widget = _mapping(widgets[0], "layout.widgets[0]")

    top_unknown = {
        key: value
        for key, value in layout.items()
        if key not in {"id", "width", "height", "widgets", "fields"}
    }
    widget_unknown = {
        key: value for key, value in widget.items() if key not in _GENERATED_WIDGET_KEYS
    }

    field_key = _field_key_for(layout, widget)
    metric = widget.get("metric") or field_key
    if not isinstance(metric, str) or not metric:
        metric = widget.get("ir_id") or layout.get("id") or "widget"

    binding: dict[str, Any] = {"metric": metric}
    field_meta = _field_metadata(layout, widget)
    for key in ("unit", "scale", "value_type"):
        if widget.get(key) is not None:
            binding[key] = widget[key]
        elif field_meta.get(key) is not None:
            binding[key] = field_meta[key]
    if widget.get("format") is not None:
        binding["format"] = widget["format"]
    field_id = widget.get("field", field_meta.get("id"))
    if isinstance(field_id, int) and not isinstance(field_id, bool):
        binding["field_id"] = field_id

    kind = widget.get("ir_kind") or _REVERSE_SCREEN_TYPES.get(
        widget.get("type"), widget.get("type") or "custom"
    )

    ir: dict[str, Any] = {
        "id": str(widget.get("ir_id") or widget.get("id") or layout.get("id") or "widget"),
        "kind": kind,
        "binding": binding,
    }
    if widget.get("label") is not None:
        ir["label"] = str(widget["label"])
    domain: dict[str, Any] = {}
    for key in ("min", "max"):
        if widget.get(key) is not None:
            domain[key] = widget[key]
    if domain:
        ir["domain"] = domain
    layout_hint: dict[str, Any] = {}
    for source_key, target_key in (
        ("x", "x"),
        ("y", "y"),
        ("w", "width"),
        ("h", "height"),
    ):
        if widget.get(source_key) is not None:
            layout_hint[target_key] = widget[source_key]
    if layout_hint:
        ir["layout"] = layout_hint
    if widget.get("font_size") is not None:
        ir["typography"] = {"size_px": widget["font_size"]}
    if widget.get("icon") is not None:
        ir["icon"] = {"name": str(widget["icon"])}
    elif widget.get("icon_asset") is not None:
        ir["icon"] = {"asset_id": str(widget["icon_asset"])}
    if widget.get("accessibility_label") is not None:
        ir["accessibility"] = {"label": str(widget["accessibility_label"])}
    if widget.get("history_seconds") is not None:
        ir["history"] = {"window_seconds": widget["history_seconds"]}
    if widget.get("action") is not None:
        action: dict[str, Any] = {"key": str(widget["action"])}
        if widget.get("action_id") is not None:
            action["action_id"] = widget["action_id"]
        ir["actions"] = [action]

    if source is None:
        ir["source"] = {
            "format": SOURCE_FORMAT_OZMA,
            "version": "1",
            "source_hash": canonical_hash(layout),
            "parsed": layout,
        }
    else:
        ir["source"] = dict(_mapping(source, "source"))

    extensions: dict[str, Any] = {}
    reserved: dict[str, Any] = {}
    if top_unknown:
        reserved["layout"] = top_unknown
    if widget_unknown:
        reserved["widget"] = widget_unknown
    if reserved:
        extensions["screen_layout"] = reserved
    ir["extensions"] = extensions
    return ir


def from_screen_layout_many(
    layout: Mapping[str, Any],
    source: Mapping[str, Any] | None = None,
) -> list[dict[str, Any]]:
    """Map every widget in a ``ScreenLayout`` to a Widget IR dict."""
    layout = _mapping(layout, "layout")
    widgets = layout.get("widgets")
    if not isinstance(widgets, list) or not widgets:
        raise DisplayIrError("screen layout must contain at least one widget")
    found: list[dict[str, Any]] = []
    for widget in widgets:
        single = {
            "id": layout.get("id"),
            "widgets": [widget],
        }
        found.append(from_screen_layout(single, source=source))
    return found


def _field_key_for(layout: Mapping[str, Any], widget: Mapping[str, Any]) -> str | None:
    meta = _field_metadata(layout, widget)
    key = meta.get("key")
    return key if isinstance(key, str) and key else None


def _field_metadata(
    layout: Mapping[str, Any], widget: Mapping[str, Any]
) -> Mapping[str, Any]:
    field_id = widget.get("field")
    if field_id is None:
        return {}
    fields = layout.get("fields")
    if isinstance(fields, list):
        for entry in fields:
            if isinstance(entry, Mapping) and entry.get("id") == field_id:
                return entry
    elif isinstance(fields, Mapping):
        for key, entry in fields.items():
            if isinstance(entry, Mapping) and entry.get("id") == field_id:
                merged = dict(entry)
                merged.setdefault("key", key)
                return merged
    return {}


__all__ = [
    "ASSET_KINDS",
    "AssetPackEntry",
    "AssetRef",
    "AssetSyncPolicy",
    "CoherenceReport",
    "DataBinding",
    "DisplayIrError",
    "FIDELITIES",
    "FontAssetRef",
    "HistoryPolicy",
    "IR_SCHEMA",
    "PACK_TYPES",
    "SOURCE_FORMAT_OZMA",
    "SYNC_MODES",
    "ShapeSignature",
    "SourceCaptureReport",
    "SourceDocument",
    "TranslationReport",
    "TypographyRef",
    "VIBE_TAXONOMY",
    "VibeProfile",
    "WIDGET_KINDS",
    "WidgetIR",
    "canonical_hash",
    "canonical_json",
    "compact_projection",
    "from_screen_layout",
    "from_screen_layout_many",
    "require_export_report",
    "to_definition_json",
    "to_screen_layout",
    "validate_asset",
    "validate_asset_pack",
    "validate_asset_pack_for_export",
    "validate_capture",
    "validate_coherence",
    "validate_ir",
    "validate_report",
    "validate_shape",
    "validate_source",
    "validate_source_capture",
    "validate_sync_policy",
    "validate_translation_report",
    "validate_vibe",
    "validate_widget_ir",
    "widget_irs_to_definitions",
]
