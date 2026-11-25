# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Deterministic shape and vibe extraction plus advisory coherence scoring.

This is the pure-Python mirror of ``ozma-display-ir/src/extract.rs``. It uses
no vision model, network access, or Rust dependency. SVG and CSS parsing is
bounded and deliberately small; SVG entities are rejected rather than
resolved.

Public functions return plain JSON-compatible mappings:

``extract_shape(widget)``
    Shape signals from a Widget IR mapping.
``extract_from_svg(text)`` / ``extract_from_css(text)``
    Shape, palette, font, and gradient signals from text.
``derive_vibe(input_data)``
    A ``VibeProfile`` derived from palette, typography, geometry, motion, and
    reference signals.
``merge_vibe_keywords(source, user, derived)``
    Merge source, user, and derived layers with explicit precedence.
``coherence_for(items, options=None)``
    Advisory coherence report across widgets, themes, or a scene.
"""

from __future__ import annotations

import copy
import math
from collections.abc import Iterable, Mapping, Sequence
from typing import Any

VIBE_TAXONOMY = "org.ozma.vibe/1"
MAX_TEXT_INPUT = 256 * 1024

_NAMED_COLORS = {
    "black": "#000000",
    "white": "#ffffff",
    "red": "#ff0000",
    "green": "#008000",
    "blue": "#0000ff",
    "yellow": "#ffff00",
    "orange": "#ffa500",
    "purple": "#800080",
    "pink": "#ffc0cb",
    "cyan": "#00ffff",
    "aqua": "#00ffff",
    "magenta": "#ff00ff",
    "fuchsia": "#ff00ff",
    "gray": "#808080",
    "grey": "#808080",
    "silver": "#c0c0c0",
    "maroon": "#800000",
    "olive": "#808000",
    "navy": "#000080",
    "teal": "#008080",
    "lime": "#00ff00",
}

_SHAPE_FIELD_ORDER = (
    "widget_kind",
    "aspect_ratio",
    "corner_radius_ratio",
    "stroke_width_ratio",
    "fill_ratio",
    "symmetry",
    "arc_start_deg",
    "arc_end_deg",
    "arc_span_deg",
    "tick_count",
    "segment_count",
    "density",
    "orientation",
    "palette_size",
    "needle_style",
    "chart_mark",
    "texture",
    "elevation",
    "glow",
    "animation",
    "confidence",
)

_SHAPE_FLOAT_FIELDS = {
    "aspect_ratio",
    "corner_radius_ratio",
    "stroke_width_ratio",
    "fill_ratio",
    "arc_start_deg",
    "arc_end_deg",
    "arc_span_deg",
    "density",
    "confidence",
}

_DIMENSION_KEYS = ("color", "typography", "form", "motion", "density")
_DIMENSION_LABELS = {
    "color": "palette",
    "typography": "typography",
    "form": "geometry",
    "motion": "motion",
    "density": "density",
}
_DIMENSION_WEIGHTS = {
    "color": 0.25,
    "typography": 0.2,
    "form": 0.2,
    "motion": 0.2,
    "density": 0.15,
}

_STANDARD_PROFILE_KEYS = (
    "taxonomy",
    "keywords",
    "mood",
    "era",
    "form",
    "material",
    "density",
    "color",
    "typography",
    "motion",
    "references",
    "ornament",
    "confidence",
    "provenance",
    "derived",
)

_ARRAY_FIELDS = {
    "keywords": "keywords",
    "mood": "mood",
    "form": "form",
    "material": "material",
    "color": "color",
    "typography": "typography",
    "motion": "motion",
    "references": "references",
    "ornament": "ornament",
}


class ShapeExtractError(ValueError):
    """Typed extraction error with a stable field name."""

    def __init__(self, field: str, message: str) -> None:
        self.field = field
        self.message = message
        super().__init__(f"invalid `{field}`: {message}")


# ---------------------------------------------------------------------------
# Deterministic primitives
# ---------------------------------------------------------------------------


def _round4(value: float) -> float:
    sign = -1.0 if value < 0 else 1.0
    rounded = math.floor(abs(value) * 10_000.0 + 0.5) / 10_000.0 * sign
    return 0.0 if rounded == 0 else rounded


def _round_u32(value: float) -> int:
    return int(math.floor(max(0.0, value) + 0.5))


def _check_input(text: str, field: str) -> None:
    if not text.strip():
        raise ShapeExtractError(field, "must not be empty")
    if len(text.encode("utf-8")) > MAX_TEXT_INPUT:
        raise ShapeExtractError(field, f"input exceeds {MAX_TEXT_INPUT} bytes")


def _parse_length(value: str) -> float | None:
    text = value.strip()
    if not text:
        return None
    end = len(text)
    for index, character in enumerate(text):
        if character.isascii() and character.isalpha() or character == "%":
            end = index
            break
    try:
        number = float(text[:end].strip())
    except ValueError:
        return None
    return number if math.isfinite(number) else None


def _parse_hex_color(text: str) -> tuple[int, int, int] | None:
    if not text.startswith("#"):
        return None
    hex_text = text[1:]
    if not hex_text or any(character not in "0123456789abcdefABCDEF" for character in hex_text):
        return None
    try:
        if len(hex_text) in (3, 4):
            return tuple(int(hex_text[index] * 2, 16) for index in range(3))  # type: ignore[return-value]
        if len(hex_text) in (6, 8):
            return (
                int(hex_text[0:2], 16),
                int(hex_text[2:4], 16),
                int(hex_text[4:6], 16),
            )
    except ValueError:
        return None
    return None


def _parse_color_component(value: str) -> int | None:
    if value.endswith("%"):
        try:
            number = float(value[:-1])
        except ValueError:
            return None
        if not math.isfinite(number):
            return None
        return int(round(min(max(number, 0.0), 100.0) * 2.55))
    try:
        number = float(value)
    except ValueError:
        return None
    if not math.isfinite(number):
        return None
    return int(round(min(max(number, 0.0), 255.0)))


def _normalize_color(value: Any) -> str | None:
    if not isinstance(value, str):
        return None
    text = value.strip().lower()
    if not text or text in {"none", "transparent", "currentcolor"} or text.startswith("url("):
        return None
    if text.startswith("#"):
        parsed = _parse_hex_color(text)
        if parsed is None:
            return None
        return "#{:02x}{:02x}{:02x}".format(*parsed)
    for prefix in ("rgba(", "rgb("):
        if text.startswith(prefix) and text.endswith(")"):
            inner = text[len(prefix) : -1]
            parts = [part for part in inner.replace("/", ",").split(",") if part.strip()]
            if len(parts) < 3:
                parts = [part for part in inner.split() if part]
            if len(parts) >= 3:
                red = _parse_color_component(parts[0].strip())
                green = _parse_color_component(parts[1].strip())
                blue = _parse_color_component(parts[2].strip())
                if red is not None and green is not None and blue is not None:
                    return f"#{red:02x}{green:02x}{blue:02x}"
    return _NAMED_COLORS.get(text)


def _push_unique(values: list[str], value: str) -> None:
    if value not in values:
        values.append(value)


def _colors_in_text(text: str) -> list[str]:
    colors: list[str] = []
    index = 0
    length = len(text)
    while index < length:
        character = text[index]
        if character == "#":
            end = index + 1
            while end < length and text[end] in "0123456789abcdefABCDEF":
                end += 1
            if end - index - 1 in (3, 4, 6, 8):
                color = _normalize_color(text[index:end])
                if color is not None:
                    _push_unique(colors, color)
            index = end
            continue
        lowered = text[index : index + 5].lower()
        if lowered.startswith("rgb(") or lowered.startswith("rgba("):
            close = text.find(")", index)
            if close == -1:
                index += 1
                continue
            color = _normalize_color(text[index : close + 1])
            if color is not None:
                _push_unique(colors, color)
            index = close + 1
            continue
        index += 1
    return colors


def _rgb_to_hsl(red: int, green: int, blue: int) -> tuple[float, float, float]:
    red_f = red / 255.0
    green_f = green / 255.0
    blue_f = blue / 255.0
    maximum = max(red_f, green_f, blue_f)
    minimum = min(red_f, green_f, blue_f)
    lightness = (maximum + minimum) / 2.0
    delta = maximum - minimum
    if delta <= 0.0:
        return (0.0, 0.0, _round4(lightness))
    saturation = (
        delta / (2.0 - maximum - minimum)
        if lightness > 0.5
        else delta / (maximum + minimum)
    )
    if maximum == red_f:
        hue = ((green_f - blue_f) / delta) % 6.0 * 60.0
    elif maximum == green_f:
        hue = ((blue_f - red_f) / delta + 2.0) * 60.0
    else:
        hue = ((red_f - green_f) / delta + 4.0) * 60.0
    return (_round4(hue), _round4(saturation), _round4(lightness))


def _orientation_for(width: float | None, height: float | None, aspect: float | None) -> str:
    if width is not None and height is not None and abs(width - height) <= 2.220446049250313e-16:
        return "square"
    if aspect is not None:
        if aspect > 1.05:
            return "horizontal"
        if aspect < 0.95:
            return "vertical"
    return "square"


def _shape_fields(shape: Mapping[str, Any]) -> set[str]:
    return {
        field
        for field in _SHAPE_FIELD_ORDER
        if shape.get(field) is not None
    }


def _shape_output(shape: Mapping[str, Any], provenance: Mapping[str, Any]) -> dict[str, Any]:
    output: dict[str, Any] = {}
    for field in _SHAPE_FIELD_ORDER:
        value = shape.get(field)
        if value is not None:
            output[field] = float(value) if field in _SHAPE_FLOAT_FIELDS else value
    if any(provenance.get(key) for key in ("methods", "source_fields", "user_fields", "derived_fields")):
        output["provenance"] = {
            "methods": list(provenance.get("methods") or []),
            "source_fields": list(provenance.get("source_fields") or []),
            "user_fields": list(provenance.get("user_fields") or []),
            "derived_fields": list(provenance.get("derived_fields") or []),
        }
    return output


def _fill_missing(target: dict[str, Any], source: Mapping[str, Any]) -> None:
    for field in _SHAPE_FIELD_ORDER:
        if target.get(field) is None and source.get(field) is not None:
            target[field] = source[field]


def _token_number(
    tokens: Mapping[str, Any], keys: Sequence[str]
) -> tuple[float, bool] | None:
    normalized_keys = {key.lower().replace("_", "-") for key in keys}
    for key, value in sorted(tokens.items()):
        if key.lower().replace("_", "-") not in normalized_keys:
            continue
        parsed = _value_number(value)
        if parsed is not None:
            return parsed
    return None


def _value_number(value: Any) -> tuple[float, bool] | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        number = float(value)
        return (number, False) if math.isfinite(number) else None
    if isinstance(value, str):
        text = value.strip()
        number = _parse_length(text)
        if number is not None:
            return (number, text.endswith("%"))
    return None


def _token_ratio(tokens: Mapping[str, Any], keys: Sequence[str]) -> float | None:
    parsed = _token_number(tokens, keys)
    if parsed is None:
        return None
    number, percent = parsed
    return _round4(number / 100.0) if percent else _round4(number)


def _collect_colors_from_value(value: Any, colors: list[str]) -> None:
    if isinstance(value, str):
        color = _normalize_color(value)
        if color is not None:
            _push_unique(colors, color)
    elif isinstance(value, list):
        for item in value:
            _collect_colors_from_value(item, colors)
    elif isinstance(value, Mapping):
        for _, item in sorted(value.items()):
            _collect_colors_from_value(item, colors)


def _arc_span(start: float | None, end: float | None) -> float | None:
    if start is None or end is None:
        return None
    return _round4(abs(end - start))


def _apply_numeric_hint(shape: dict[str, Any], key: str, value: Any) -> None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return
    number = float(value)
    if not math.isfinite(number) or number < 0:
        return
    mapping = {
        "tick_count": "tick_count",
        "tick-count": "tick_count",
        "segment_count": "segment_count",
        "segment-count": "segment_count",
        "arc_start_deg": "arc_start_deg",
        "arc_end_deg": "arc_end_deg",
        "arc_span_deg": "arc_span_deg",
        "arc-span-deg": "arc_span_deg",
    }
    field = mapping.get(key)
    if field is not None and shape.get(field) is None:
        shape[field] = _round_u32(number) if field in {"tick_count", "segment_count"} else _round4(number)


# ---------------------------------------------------------------------------
# Shape extraction
# ---------------------------------------------------------------------------


def extract_shape(widget: Mapping[str, Any]) -> dict[str, Any]:
    """Extract a shape signature from a Widget IR mapping.

    Values already present on ``widget["shape"]`` are preserved verbatim.
    """
    existing = copy.deepcopy(widget.get("shape") or {})
    if not isinstance(existing, Mapping):
        existing = {}
    source_fields = _shape_fields(existing)
    extracted: dict[str, Any] = {"widget_kind": widget.get("kind")}
    methods: set[str] = {"widget_ir"}

    palette: list[str] = []
    layout = widget.get("layout")
    if isinstance(layout, Mapping):
        methods.add("layout")
        width = layout.get("width")
        height = layout.get("height")
        aspect = layout.get("aspect_ratio")
        if aspect is None and isinstance(width, (int, float)) and isinstance(height, (int, float)):
            if width > 0 and height > 0:
                aspect = _round4(float(width) / float(height))
        extracted["aspect_ratio"] = aspect
        if (
            isinstance(width, (int, float))
            and isinstance(height, (int, float))
            and width > 0
            and height > 0
        ):
            extracted["density"] = _round4(1.0 / ((float(width) * float(height)) / 10_000.0))
        extracted["orientation"] = _orientation_for(
            float(width) if isinstance(width, (int, float)) else None,
            float(height) if isinstance(height, (int, float)) else None,
            float(aspect) if isinstance(aspect, (int, float)) else None,
        )

    style = widget.get("style")
    if isinstance(style, Mapping):
        methods.add("style")
        tokens = style.get("tokens") or {}
        if not isinstance(tokens, Mapping):
            tokens = {}
        reference = 100.0
        if isinstance(layout, Mapping):
            width = layout.get("width")
            height = layout.get("height")
            if isinstance(width, (int, float)) and isinstance(height, (int, float)):
                reference = float(min(width, height)) if min(width, height) > 0 else 100.0
        ratio = _token_ratio(tokens, ("aspect-ratio", "aspect_ratio", "aspect"))
        if ratio is not None:
            extracted["aspect_ratio"] = ratio
        radius = _token_number(
            tokens,
            ("border-radius", "border_radius", "corner-radius", "corner_radius", "radius"),
        )
        if radius is not None:
            value, percent = radius
            extracted["corner_radius_ratio"] = _round4(value / 100.0) if percent else _round4(value / reference)
        stroke = _token_number(
            tokens, ("stroke-width", "stroke_width", "border-width", "border_width")
        )
        if stroke is not None:
            value, percent = stroke
            extracted["stroke_width_ratio"] = _round4(value / 100.0) if percent else _round4(value / reference)
        fill = _token_number(tokens, ("fill-ratio", "fill_ratio"))
        if fill is not None:
            extracted["fill_ratio"] = _round4(fill[0])
        ticks = _token_number(tokens, ("tick-count", "tick_count"))
        if ticks is not None:
            extracted["tick_count"] = _round_u32(ticks[0])
        segments = _token_number(tokens, ("segment-count", "segment_count"))
        if segments is not None:
            extracted["segment_count"] = _round_u32(segments[0])
        arc_start = _token_number(tokens, ("arc-start", "arc_start"))
        if arc_start is not None:
            extracted["arc_start_deg"] = _round4(arc_start[0])
        arc_end = _token_number(tokens, ("arc-end", "arc_end"))
        if arc_end is not None:
            extracted["arc_end_deg"] = _round4(arc_end[0])
        arc_span = _token_number(tokens, ("arc-span", "arc_span"))
        if arc_span is not None:
            extracted["arc_span_deg"] = _round4(arc_span[0])
        needle = _token_number(tokens, ("needle-width", "needle_width"))
        if needle is not None and needle[0] >= 4.0:
            extracted["needle_style"] = "modern"
        for class_name in style.get("classes") or []:
            lowered = str(class_name).lower()
            if lowered in {"rounded", "pill", "circle"}:
                if extracted.get("corner_radius_ratio") is None:
                    extracted["corner_radius_ratio"] = 0.5
                if extracted.get("symmetry") is None:
                    extracted["symmetry"] = "radial"
            elif lowered in {"sharp", "square"}:
                if extracted.get("corner_radius_ratio") is None:
                    extracted["corner_radius_ratio"] = 0.0
            elif lowered in {"dense", "dashboard"}:
                if extracted.get("density") is None:
                    extracted["density"] = 10.0
            elif lowered in {"sparse", "minimal"} and extracted.get("density") is None:
                extracted["density"] = 0.25
        for value in tokens.values():
            _collect_colors_from_value(value, palette)

    domain = widget.get("domain")
    if isinstance(domain, Mapping):
        for threshold in domain.get("thresholds") or []:
            if isinstance(threshold, Mapping):
                color = _normalize_color(threshold.get("color"))
                if color is not None:
                    _push_unique(palette, color)
    if palette:
        extracted["palette_size"] = len(palette)

    extensions = widget.get("extensions") or {}
    if isinstance(extensions, Mapping):
        for key in (
            "tick_count",
            "tick-count",
            "segment_count",
            "segment-count",
            "arc_start_deg",
            "arc_end_deg",
            "arc_span_deg",
        ):
            if key in extensions:
                _apply_numeric_hint(extracted, key, extensions[key])
        shape_extension = extensions.get("shape")
        if isinstance(shape_extension, Mapping):
            _fill_missing(extracted, shape_extension)

    kind = widget.get("kind")
    if extracted.get("symmetry") is None:
        if kind == "gauge":
            extracted["symmetry"] = "radial"
        elif kind == "bar":
            extracted["symmetry"] = "horizontal"
    if extracted.get("orientation") is None:
        if kind == "gauge":
            extracted["orientation"] = "radial"
        elif kind == "bar":
            extracted["orientation"] = "horizontal"
    if extracted.get("tick_count") is None and kind == "gauge" and isinstance(domain, Mapping):
        thresholds = domain.get("thresholds") or []
        if thresholds:
            extracted["tick_count"] = len(thresholds) + 1
    if extracted.get("arc_span_deg") is None:
        span = _arc_span(extracted.get("arc_start_deg"), extracted.get("arc_end_deg"))
        if span is not None:
            extracted["arc_span_deg"] = span
    if extracted.get("density") is not None:
        elements = extracted.get("segment_count") or extracted.get("tick_count") or 1
        extracted["density"] = _round4(float(extracted["density"]) * float(max(1, elements)))

    existing_provenance = existing.get("provenance") if isinstance(existing, Mapping) else None
    if isinstance(existing_provenance, Mapping):
        methods.update(existing_provenance.get("methods") or [])
    result = copy.deepcopy(existing)
    user_fields = list(
        existing_provenance.get("user_fields") or []
        if isinstance(existing_provenance, Mapping)
        else []
    )
    provenance_source_fields = list(
        existing_provenance.get("source_fields") or []
        if isinstance(existing_provenance, Mapping)
        else []
    )
    for field in source_fields:
        if field not in user_fields:
            provenance_source_fields.append(field)
    provenance_source_fields = sorted(set(provenance_source_fields))
    user_fields = sorted(set(user_fields))
    _fill_missing(result, extracted)
    if result.get("arc_span_deg") is None:
        result["arc_span_deg"] = _arc_span(result.get("arc_start_deg"), result.get("arc_end_deg"))
    derived_fields = _shape_fields(result) - set(provenance_source_fields) - set(user_fields)
    if result.get("confidence") is None:
        base = 0.55
        base += 0.1 if result.get("aspect_ratio") is not None else 0.0
        base += 0.1 if result.get("density") is not None else 0.0
        base += 0.05 if result.get("palette_size") is not None else 0.0
        base += 0.05 if result.get("corner_radius_ratio") is not None else 0.0
        result["confidence"] = _round4(min(base, 0.95))
    provenance = {
        "methods": sorted(methods),
        "source_fields": provenance_source_fields,
        "user_fields": user_fields,
        "derived_fields": sorted(derived_fields),
    }
    return _shape_output(result, provenance)


# ---------------------------------------------------------------------------
# Bounded SVG and CSS parsing
# ---------------------------------------------------------------------------


def _strip_comments(text: str, field: str) -> str:
    output: list[str] = []
    rest = text
    while "<!--" in rest:
        start = rest.find("<!--")
        output.append(rest[:start])
        after = rest[start + 4 :]
        end = after.find("-->")
        if end == -1:
            raise ShapeExtractError(field, "unterminated comment")
        rest = after[end + 3 :]
    output.append(rest)
    return "".join(output)


def _find_tag_end(text: str) -> int | None:
    quote: str | None = None
    for index in range(1, len(text)):
        character = text[index]
        if quote is not None:
            if character == quote:
                quote = None
        elif character in {'"', "'"}:
            quote = character
        elif character == ">":
            return index
    return None


def _split_tags(text: str) -> list[str]:
    tags: list[str] = []
    position = 0
    while "<" in text[position:]:
        start = text.find("<", position)
        end = _find_tag_end(text[start:])
        if end is None:
            break
        absolute_end = start + end
        tags.append(text[start : absolute_end + 1])
        position = absolute_end + 1
    return tags


def _tag_name(tag: str) -> str:
    if not tag.startswith("<"):
        return ""
    end = 1
    while end < len(tag) and not tag[end].isspace() and tag[end] not in ">/":
        end += 1
    return tag[1:end]


def _attribute_map(tag: str) -> dict[str, str]:
    index = 1
    while index < len(tag) and not tag[index].isspace() and tag[index] not in ">/":
        index += 1
    attrs: dict[str, str] = {}
    while index < len(tag):
        while index < len(tag) and (tag[index].isspace() or tag[index] == "/"):
            index += 1
        if index >= len(tag) or tag[index] == ">":
            break
        name_start = index
        while (
            index < len(tag)
            and not tag[index].isspace()
            and tag[index] not in "=>/"
        ):
            index += 1
        name = tag[name_start:index].lower()
        while index < len(tag) and tag[index].isspace():
            index += 1
        if index < len(tag) and tag[index] == "=":
            index += 1
            while index < len(tag) and tag[index].isspace():
                index += 1
            if index < len(tag) and tag[index] in {'"', "'"}:
                quote = tag[index]
                index += 1
                value_start = index
                while index < len(tag) and tag[index] != quote:
                    index += 1
                value = tag[value_start:index]
                if index < len(tag):
                    index += 1
                if name:
                    attrs[name] = value
            else:
                value_start = index
                while index < len(tag) and not tag[index].isspace() and tag[index] != ">":
                    index += 1
                if name:
                    attrs[name] = tag[value_start:index]
        elif name:
            attrs[name] = ""
    return attrs


def _parse_viewbox(value: str) -> tuple[float, float, float, float] | None:
    parts = [part for part in value.replace(",", " ").split() if part]
    if len(parts) != 4:
        return None
    try:
        numbers = tuple(float(part) for part in parts)
    except ValueError:
        return None
    if all(math.isfinite(number) for number in numbers):
        return numbers  # type: ignore[return-value]
    return None


def _path_tokens(data: str) -> list[tuple[str, Any]]:
    tokens: list[tuple[str, Any]] = []
    index = 0
    length = len(data)
    allowed = "MmLlHhVvCcSsQqTtAaZz"
    while index < length:
        character = data[index]
        if character.isspace() or character == ",":
            index += 1
            continue
        if character.isascii() and character.isalpha():
            if character not in allowed:
                raise ShapeExtractError("svg.path", f"unknown path command `{character}`")
            tokens.append(("command", character))
            index += 1
            continue
        start = index
        if character in "+-":
            index += 1
        while index < length and data[index].isdigit():
            index += 1
        if index < length and data[index] == ".":
            index += 1
            while index < length and data[index].isdigit():
                index += 1
        if index < length and data[index] in "eE":
            index += 1
            if index < length and data[index] in "+-":
                index += 1
            while index < length and data[index].isdigit():
                index += 1
        if start == index:
            raise ShapeExtractError("svg.path", "invalid path token")
        try:
            number = float(data[start:index])
        except ValueError as error:
            raise ShapeExtractError("svg.path", "invalid path number") from error
        if not math.isfinite(number):
            raise ShapeExtractError("svg.path", "path numbers must be finite")
        tokens.append(("number", number))
    return tokens


def parse_svg_path(data: str) -> dict[str, int]:
    """Parse SVG path data, validating command syntax and arc flags."""
    tokens = _path_tokens(data)
    stats = {"commands": 0, "segments": 0, "arcs": 0, "large_arc_flags": 0, "sweep_flags": 0}
    index = 0
    while index < len(tokens):
        kind, command = tokens[index]
        if kind != "command":
            raise ShapeExtractError("svg.path", "path must start with a command")
        index += 1
        stats["commands"] += 1
        upper = command.upper()
        arity = {"Z": 0, "M": 2, "L": 2, "T": 2, "H": 1, "V": 1, "C": 6, "S": 4, "Q": 4, "A": 7}.get(upper)
        if arity is None:
            raise ShapeExtractError("svg.path", f"unknown path command `{command}`")
        if arity == 0:
            continue
        group_start = index
        while index < len(tokens) and tokens[index][0] != "command":
            group_end = index + arity
            if group_end > len(tokens):
                raise ShapeExtractError("svg.path", "incomplete path command")
            values = [token[1] for token in tokens[index:group_end] if token[0] == "number"]
            if len(values) != arity:
                raise ShapeExtractError("svg.path", "expected numeric path parameters")
            if command.lower() == "a":
                if values[3] not in (0.0, 1.0) or values[4] not in (0.0, 1.0):
                    raise ShapeExtractError("svg.path", "arc flags must be 0 or 1")
                stats["arcs"] += 1
                if values[3] == 1.0:
                    stats["large_arc_flags"] += 1
                if values[4] == 1.0:
                    stats["sweep_flags"] += 1
            if command.upper() not in {"M", "Z"}:
                stats["segments"] += 1
            index = group_end
        if index == group_start:
            raise ShapeExtractError("svg.path", "path command has no parameters")
    return stats


def _style_declarations(style: str) -> list[tuple[str, str]]:
    declarations: list[tuple[str, str]] = []
    for part in style.split(";"):
        if ":" not in part:
            continue
        property_name, value = part.split(":", 1)
        property_name = property_name.strip().lower()
        value = value.strip()
        if property_name and value:
            declarations.append((property_name, value))
    return declarations


def _split_font_families(value: str) -> list[str]:
    families: list[str] = []
    for part in value.split(","):
        family = part.strip().strip("\"'")
        if family:
            families.append(family)
    return families


def extract_from_svg(text: str) -> dict[str, Any]:
    """Parse an SVG document and return shape, palette, and font signals."""
    _check_input(text, "svg")
    lowered = text.lower()
    if "<!entity" in lowered:
        raise ShapeExtractError("svg", "XML entities are not supported")
    if "<!doctype" in lowered and ("system" in lowered or "public" in lowered):
        raise ShapeExtractError("svg", "external DTD subsets are not supported")
    cleaned = _strip_comments(text, "svg")
    tags = _split_tags(cleaned)
    root = next((tag for tag in tags if _tag_name(tag).lower() == "svg"), None)
    if root is None:
        raise ShapeExtractError("svg", "missing <svg> root element")
    if not root.rstrip().endswith("/>") and "</svg" not in lowered:
        raise ShapeExtractError("svg", "missing </svg> closing tag")

    root_attrs = _attribute_map(root)
    viewbox = _parse_viewbox(root_attrs.get("viewbox", ""))
    width = _parse_length(root_attrs.get("width", ""))
    height = _parse_length(root_attrs.get("height", ""))
    if viewbox is not None:
        box_width, box_height = viewbox[2], viewbox[3]
        if box_width <= 0 or box_height <= 0:
            raise ShapeExtractError("svg", "viewBox must have positive dimensions")
    elif width is not None and height is not None and width > 0 and height > 0:
        box_width, box_height = width, height
    else:
        raise ShapeExtractError("svg", "expected a viewBox or positive width and height")
    shorter = min(box_width, box_height)

    shape: dict[str, Any] = {
        "aspect_ratio": _round4(box_width / box_height),
        "orientation": (
            "square"
            if abs(box_width - box_height) <= 2.220446049250313e-16
            else "horizontal"
            if box_width > box_height
            else "vertical"
        ),
    }
    colors: list[str] = []
    fonts: list[str] = []
    gradients: set[str] = set()
    animated = False
    max_rx = 0.0
    max_ry = 0.0
    max_stroke = 0.0
    segments = 0
    arcs = 0
    large_arc = False
    fill_ratio: float | None = None
    has_circle = False
    has_path = False

    for tag in tags:
        name = _tag_name(tag)
        if not name or name.startswith(("!", "?", "/")):
            continue
        attrs = _attribute_map(tag)
        if any(key in attrs for key in ("fill", "stroke", "stop-color")):
            for key in ("fill", "stroke", "stop-color"):
                color = _normalize_color(attrs.get(key))
                if color is not None:
                    _push_unique(colors, color)
        style = attrs.get("style")
        if style:
            for _, value in _style_declarations(style):
                for color in _colors_in_text(value):
                    _push_unique(colors, color)
                if "linear-gradient(" in value.lower():
                    gradients.add("linear-gradient")
                if "radial-gradient(" in value.lower():
                    gradients.add("radial-gradient")
        family = attrs.get("font-family")
        if family:
            for font in _split_font_families(family):
                _push_unique(fonts, font)
        if name.lower() == "lineargradient":
            gradients.add("linear-gradient")
        if name.lower() == "radialgradient":
            gradients.add("radial-gradient")
        if name.lower().startswith("animate"):
            animated = True
        if name.lower() == "circle":
            has_circle = True
        rx = _parse_length(attrs.get("rx", ""))
        if rx is not None:
            max_rx = max(max_rx, rx)
        ry = _parse_length(attrs.get("ry", ""))
        if ry is not None:
            max_ry = max(max_ry, ry)
        stroke = _parse_length(attrs.get("stroke-width", ""))
        if stroke is not None:
            max_stroke = max(max_stroke, stroke)
        if name.lower() == "rect":
            w = _parse_length(attrs.get("width", ""))
            h = _parse_length(attrs.get("height", ""))
            if w is not None and h is not None and w > 0 and h > 0:
                ratio = min(max((w * h) / (box_width * box_height), 0.0), 1.0)
                fill_ratio = _round4(ratio if fill_ratio is None else max(fill_ratio, ratio))
        if name.lower() == "path" and attrs.get("d") is not None:
            has_path = True
            stats = parse_svg_path(attrs["d"])
            segments += stats["segments"]
            arcs += stats["arcs"]
            large_arc = large_arc or stats["large_arc_flags"] > 0

    if shorter > 0:
        if max_rx > 0 or max_ry > 0:
            shape["corner_radius_ratio"] = _round4(max(max_rx, max_ry) / shorter)
        if max_stroke > 0:
            shape["stroke_width_ratio"] = _round4(max_stroke / shorter)
    if fill_ratio is not None:
        shape["fill_ratio"] = fill_ratio
    if segments > 0:
        shape["segment_count"] = segments
    if arcs > 0:
        shape["arc_span_deg"] = 360.0 if large_arc else 180.0
    shape["symmetry"] = (
        "radial" if has_circle or shape.get("corner_radius_ratio", 0.0) >= 0.5 else "none"
    )
    if has_path and shape.get("orientation") == "square":
        shape["orientation"] = "radial"
    if colors:
        shape["palette_size"] = len(colors)
    provenance = {
        "methods": ["svg"],
        "source_fields": [],
        "user_fields": [],
        "derived_fields": sorted(_shape_fields(shape)),
    }
    shape["confidence"] = _round4(
        min((0.8 if viewbox is not None else 0.65) + (0.05 if colors else 0.0), 0.95)
    )
    return {
        "shape": _shape_output(shape, provenance),
        "colors": colors,
        "fonts": sorted(set(fonts)),
        "gradients": sorted(gradients),
        "animated": animated,
    }


def _validate_css(text: str) -> None:
    cleaned = _strip_comments(text, "css")
    depth = 0
    quote: str | None = None
    for character in cleaned:
        if quote is not None:
            if character == quote:
                quote = None
        elif character in {'"', "'"}:
            quote = character
        elif character == "{":
            depth += 1
        elif character == "}":
            depth -= 1
            if depth < 0:
                raise ShapeExtractError("css", "unbalanced closing brace")
    if quote is not None:
        raise ShapeExtractError("css", "unterminated string")
    if depth != 0:
        raise ShapeExtractError("css", "unbalanced braces")


def _css_declarations(text: str) -> list[tuple[str, str]]:
    cleaned = _strip_comments(text, "css")
    declarations: list[tuple[str, str]] = []
    depth = 0
    buffer: list[str] = []

    def push() -> None:
        raw = "".join(buffer).strip()
        if not raw or ":" not in raw:
            return
        property_name, value = raw.split(":", 1)
        property_name = property_name.strip().lower()
        value = value.strip()
        if property_name and value and "{" not in property_name:
            declarations.append((property_name, value))

    for character in cleaned:
        if character == "{":
            depth += 1
            buffer.clear()
        elif character == "}":
            if depth > 0:
                push()
                depth -= 1
            buffer.clear()
        elif character == ";" and depth > 0:
            push()
            buffer.clear()
        elif depth > 0:
            buffer.append(character)
    if depth > 0:
        push()
    return declarations


def _css_first_length(value: str) -> tuple[float, bool] | None:
    parts = [part for part in value.replace(",", " ").split() if part]
    if not parts:
        return None
    token = parts[0]
    number = _parse_length(token)
    if number is None:
        return None
    return number, token.endswith("%")


def _parse_ratio(value: str) -> tuple[float, float] | None:
    parts = value.split("/")
    if len(parts) < 2:
        return None
    width = _parse_length(parts[0])
    height = _parse_length(parts[1])
    if width is not None and height is not None and width > 0 and height > 0:
        return width, height
    return None


def _declaration_number(declarations: Sequence[tuple[str, str]], names: Sequence[str]) -> float | None:
    for property_name, value in declarations:
        if property_name in names:
            parsed = _css_first_length(value)
            if parsed is not None:
                return parsed[0]
    return None


def extract_from_css(text: str) -> dict[str, Any]:
    """Parse a CSS stylesheet and return shape, palette, and font signals."""
    _check_input(text, "css")
    _validate_css(text)
    declarations = _css_declarations(text)
    shape: dict[str, Any] = {}
    colors: list[str] = []
    fonts: list[str] = []
    gradients: set[str] = set()
    animated = False

    width = _declaration_number(declarations, ("width",))
    height = _declaration_number(declarations, ("height",))
    if width is not None and height is not None and width > 0 and height > 0:
        shape["aspect_ratio"] = _round4(width / height)
        shape["orientation"] = _orientation_for(width, height, shape["aspect_ratio"])
    reference = min(width, height) if width is not None and height is not None and width > 0 and height > 0 else 100.0

    for property_name, value in declarations:
        lowered = value.lower()
        if property_name == "aspect-ratio":
            ratio = _parse_ratio(value)
            if ratio is not None:
                shape["aspect_ratio"] = _round4(ratio[0] / ratio[1])
        elif property_name in {"border-radius", "corner-radius", "radius"}:
            parsed = _css_first_length(value)
            if parsed is not None:
                number, percent = parsed
                shape["corner_radius_ratio"] = (
                    _round4(number / 100.0) if percent else _round4(number / reference)
                )
        elif property_name in {"border-width", "stroke-width"}:
            parsed = _css_first_length(value)
            if parsed is not None:
                number, percent = parsed
                shape["stroke_width_ratio"] = (
                    _round4(number / 100.0) if percent else _round4(number / reference)
                )
        elif property_name == "fill-ratio":
            parsed = _css_first_length(value)
            if parsed is not None:
                shape["fill_ratio"] = _round4(parsed[0])
        elif property_name == "tick-count":
            parsed = _css_first_length(value)
            if parsed is not None:
                shape["tick_count"] = _round_u32(parsed[0])
        elif property_name == "segment-count":
            parsed = _css_first_length(value)
            if parsed is not None:
                shape["segment_count"] = _round_u32(parsed[0])
        elif property_name == "font-family":
            for font in _split_font_families(value):
                _push_unique(fonts, font)
        elif property_name in {"animation", "animation-name", "transition", "transition-property"}:
            if lowered.strip() and lowered.strip() not in {"none", "0s"}:
                animated = True
        for color in _colors_in_text(value):
            _push_unique(colors, color)
        if "linear-gradient(" in lowered:
            gradients.add("linear-gradient")
        if "radial-gradient(" in lowered:
            gradients.add("radial-gradient")

    if colors:
        shape["palette_size"] = len(colors)
    provenance = {
        "methods": ["css"],
        "source_fields": [],
        "user_fields": [],
        "derived_fields": sorted(_shape_fields(shape)),
    }
    shape["confidence"] = _round4(
        min(0.7 + (0.05 if colors else 0.0) + (0.05 if shape.get("aspect_ratio") is not None else 0.0), 0.95)
    )
    return {
        "shape": _shape_output(shape, provenance),
        "colors": colors,
        "fonts": sorted(set(fonts)),
        "gradients": sorted(gradients),
        "animated": animated,
    }


# ---------------------------------------------------------------------------
# Vibe derivation and keyword merging
# ---------------------------------------------------------------------------


def _default_profile() -> dict[str, Any]:
    return {
        "taxonomy": VIBE_TAXONOMY,
        "keywords": [],
        "mood": [],
        "form": [],
        "material": [],
        "color": [],
        "typography": [],
        "motion": [],
        "references": [],
        "ornament": [],
        "confidence": 1.0,
        "provenance": {
            "source_keywords": [],
            "derived_keywords": [],
            "user_keywords": [],
            "methods": [],
        },
    }


def _as_profile(value: Mapping[str, Any] | None) -> dict[str, Any]:
    profile = _default_profile()
    if isinstance(value, Mapping):
        for key, item in value.items():
            profile[key] = copy.deepcopy(item)
    return profile


def _profile_keywords(profile: Mapping[str, Any]) -> list[str]:
    values: list[str] = []
    for field in ("keywords", "mood", "form", "material", "color", "typography", "motion", "references", "ornament"):
        for value in profile.get(field) or []:
            if isinstance(value, str) and value.lower() not in {existing.lower() for existing in values}:
                values.append(value)
    for key in ("era", "density"):
        value = profile.get(key)
        if isinstance(value, str) and value.lower() not in {existing.lower() for existing in values}:
            values.append(value)
    return values


def _dedupe(values: Iterable[str]) -> list[str]:
    result: list[str] = []
    for value in values:
        if value.lower() not in {existing.lower() for existing in result}:
            result.append(value)
    return result


def _source_layer_keywords(profile: Mapping[str, Any]) -> list[str]:
    provenance = profile.get("provenance") or {}
    values = provenance.get("source_keywords") if isinstance(provenance, Mapping) else None
    return _dedupe(values) if values else _profile_keywords(profile)


def _user_layer_keywords(profile: Mapping[str, Any]) -> list[str]:
    provenance = profile.get("provenance") or {}
    values = provenance.get("user_keywords") if isinstance(provenance, Mapping) else None
    return _dedupe(values) if values else _profile_keywords(profile)


def _keyword_axis(keyword: str) -> tuple[str, str] | None:
    return {
        "rounded": ("corner", "rounded"),
        "soft": ("corner", "rounded"),
        "sharp": ("corner", "sharp"),
        "square": ("corner", "sharp"),
        "circular": ("form", "circular"),
        "symmetric": ("symmetry", "symmetric"),
        "asymmetric": ("symmetry", "asymmetric"),
        "sparse": ("density", "sparse"),
        "balanced": ("density", "balanced"),
        "dense": ("density", "dense"),
        "dashboard": ("density", "dense"),
        "static": ("motion", "static"),
        "animated": ("motion", "animated"),
        "warm": ("temperature", "warm"),
        "cool": ("temperature", "cool"),
        "neutral": ("temperature", "neutral"),
        "monochrome": ("vibrancy", "monochrome"),
        "pastel": ("vibrancy", "pastel"),
        "neon": ("vibrancy", "neon"),
        "high_contrast": ("contrast", "high"),
        "low_contrast": ("contrast", "low"),
        "serif": ("type_style", "serif"),
        "geometric_sans": ("type_style", "sans"),
        "humanist_sans": ("type_style", "sans"),
        "sans_serif": ("type_style", "sans"),
        "condensed": ("type_width", "condensed"),
        "expanded": ("type_width", "expanded"),
        "light": ("weight", "light"),
        "bold": ("weight", "bold"),
        "minimal": ("ornament", "minimal"),
        "decorative": ("ornament", "decorative"),
        "skeuomorphic": ("ornament", "decorative"),
        "brutalist": ("ornament", "decorative"),
    }.get(keyword.lower())


def _keywords_conflict(left: str, right: str) -> bool:
    left_axis = _keyword_axis(left)
    right_axis = _keyword_axis(right)
    if left_axis is None or right_axis is None:
        return False
    return left_axis[0] == right_axis[0] and left_axis[1] != right_axis[1]


def _remove_conflicting(profile: dict[str, Any], keyword: str) -> None:
    for field in _ARRAY_FIELDS:
        values = profile.get(field) or []
        profile[field] = [value for value in values if not _keywords_conflict(value, keyword)]
    density = profile.get("density")
    if isinstance(density, str) and _keywords_conflict(density, keyword):
        profile["density"] = None


def _append_unique(target: list[str], values: Iterable[str]) -> None:
    for value in values:
        if value.lower() not in {existing.lower() for existing in target}:
            target.append(value)


def _copy_layer(profile: dict[str, Any], layer: Mapping[str, Any]) -> None:
    for field in _ARRAY_FIELDS:
        target = profile.setdefault(field, [])
        _append_unique(target, layer.get(field) or [])


def _merge_user_layer(profile: dict[str, Any], user: Mapping[str, Any]) -> None:
    for keyword in _user_layer_keywords(user):
        _remove_conflicting(profile, keyword)
    _copy_layer(profile, user)
    if user.get("density") is not None:
        profile["density"] = user.get("density")
    if user.get("era") is not None:
        profile["era"] = user.get("era")
    for key, value in user.items():
        if key not in _STANDARD_PROFILE_KEYS:
            profile[key] = copy.deepcopy(value)
    taxonomy = user.get("taxonomy")
    if isinstance(taxonomy, str) and taxonomy.strip():
        profile["taxonomy"] = taxonomy


def _add_keyword(profile: dict[str, Any], field: str, keyword: str) -> None:
    if field == "density":
        profile["density"] = keyword
        return
    target = profile.setdefault(field if field in _ARRAY_FIELDS else "keywords", [])
    if keyword.lower() not in {existing.lower() for existing in target}:
        target.append(keyword)
    if keyword.lower() not in {existing.lower() for existing in profile.setdefault("keywords", [])}:
        profile["keywords"].append(keyword)


def merge_vibe_keywords(
    source: Mapping[str, Any] | None = None,
    user: Mapping[str, Any] | None = None,
    derived: Sequence[Mapping[str, Any]] | None = None,
) -> dict[str, Any]:
    """Merge source, user, and derived vibe layers.

    Source and user keywords are preserved verbatim. Derived keywords are
    appended only when they do not duplicate or contradict an existing
    keyword. The strongest layer wins: user over source over derived.
    """
    profile = _as_profile(source)
    if user is not None:
        _merge_user_layer(profile, user)
    if not isinstance(profile.get("taxonomy"), str) or not profile["taxonomy"].strip():
        profile["taxonomy"] = VIBE_TAXONOMY

    candidates = list(derived or [])
    normalized: list[dict[str, Any]] = []
    for candidate in candidates:
        if not isinstance(candidate, Mapping):
            continue
        keyword = str(candidate.get("keyword") or "").strip()
        if not keyword:
            continue
        confidence = candidate.get("confidence", 1.0)
        confidence = float(confidence) if isinstance(confidence, (int, float)) else 0.0
        if not math.isfinite(confidence):
            confidence = 0.0
        confidence = min(max(confidence, 0.0), 1.0)
        normalized.append(
            {
                "keyword": keyword,
                "field": str(candidate.get("field") or "").strip() or "keywords",
                "confidence": _round4(confidence),
                "method": str(candidate.get("method") or "").strip() or "derived",
            }
        )
    accepted: list[dict[str, Any]] = []
    for candidate in normalized:
        keyword = candidate["keyword"]
        existing_keywords = _profile_keywords(profile)
        conflicts = any(_keywords_conflict(existing, keyword) for existing in existing_keywords)
        duplicate = any(existing.lower() == keyword.lower() for existing in existing_keywords)
        if conflicts or duplicate:
            continue
        if not any(entry["keyword"].lower() == keyword.lower() for entry in accepted):
            accepted.append(copy.deepcopy(candidate))
    accepted.sort(key=lambda entry: (entry["field"], entry["keyword"]))
    for entry in accepted:
        _add_keyword(profile, entry["field"], entry["keyword"])

    if accepted:
        profile["derived"] = accepted
    else:
        profile.pop("derived", None)

    methods: set[str] = set()
    for layer in (source, user):
        if isinstance(layer, Mapping):
            provenance = layer.get("provenance") or {}
            if isinstance(provenance, Mapping):
                methods.update(str(method) for method in provenance.get("methods") or [])
    for candidate in normalized:
        method = candidate["method"]
        if method:
            methods.add(method)
    profile["provenance"] = {
        "source_keywords": _source_layer_keywords(source) if isinstance(source, Mapping) else [],
        "derived_keywords": [entry["keyword"] for entry in accepted],
        "user_keywords": _user_layer_keywords(user) if isinstance(user, Mapping) else [],
        "methods": sorted(methods),
    }

    if isinstance(user, Mapping):
        confidence = user.get("confidence", 1.0)
        profile["confidence"] = float(confidence) if isinstance(confidence, (int, float)) else 1.0
    elif isinstance(source, Mapping):
        confidence = source.get("confidence", 1.0)
        profile["confidence"] = float(confidence) if isinstance(confidence, (int, float)) else 1.0
    elif not accepted:
        profile["confidence"] = 0.0
    else:
        total = sum(float(entry["confidence"]) for entry in accepted)
        profile["confidence"] = _round4(total / len(accepted))
    for optional in ("era", "density"):
        if profile.get(optional) is None:
            profile.pop(optional, None)
    return profile


def _push_derived(
    derived: list[dict[str, Any]],
    keyword: str,
    field: str,
    confidence: float,
    method: str,
) -> None:
    if any(
        entry["keyword"].lower() == keyword.lower() and entry["field"] == field
        for entry in derived
    ):
        return
    derived.append(
        {
            "keyword": keyword,
            "field": field,
            "confidence": _round4(confidence),
            "method": method,
        }
    )


def _derive_palette_keywords(colors: Sequence[Any], derived: list[dict[str, Any]]) -> None:
    parsed: list[tuple[float, float, float]] = []
    for color in colors:
        normalized = _normalize_color(color)
        if normalized is None:
            continue
        components = _parse_hex_color(normalized)
        if components is not None:
            parsed.append(_rgb_to_hsl(*components))
    if not parsed:
        return
    count = float(len(parsed))
    warm = cool = neutral = 0
    saturation_total = 0.0
    lightness_total = 0.0
    lightness_min = 1.0
    lightness_max = 0.0
    phosphor = False
    amber = False
    for hue, saturation, lightness in parsed:
        saturation_total += saturation
        lightness_total += lightness
        lightness_min = min(lightness_min, lightness)
        lightness_max = max(lightness_max, lightness)
        if saturation < 0.12:
            neutral += 1
        elif hue < 70.0 or hue >= 300.0:
            warm += 1
        else:
            cool += 1
        if 90.0 <= hue <= 160.0 and saturation > 0.5:
            phosphor = True
        if 25.0 <= hue <= 65.0 and saturation > 0.5:
            amber = True
    if warm >= cool and warm >= neutral:
        dominant, matches = "warm", warm
    elif cool >= neutral:
        dominant, matches = "cool", cool
    else:
        dominant, matches = "neutral", neutral
    _push_derived(derived, dominant, "color", 0.5 + 0.4 * (matches / count), "palette")
    saturation_average = saturation_total / count
    lightness_average = lightness_total / count
    if saturation_average > 0.7 and lightness_average > 0.4:
        _push_derived(derived, "neon", "color", 0.75, "palette")
    if saturation_average < 0.25 and lightness_average > 0.7:
        _push_derived(derived, "pastel", "color", 0.7, "palette")
    if neutral / count >= 0.75:
        _push_derived(derived, "monochrome", "color", 0.75, "palette")
    if lightness_max - lightness_min > 0.55:
        _push_derived(derived, "high_contrast", "color", 0.7, "palette")
    if phosphor:
        _push_derived(derived, "phosphor_green", "color", 0.7, "palette")
    if amber:
        _push_derived(derived, "amber", "color", 0.65, "palette")


def _is_known_font(family: str) -> bool:
    return any(
        name in family
        for name in (
            "inter",
            "helvetica",
            "arial",
            "roboto",
            "lato",
            "open sans",
            "ibm plex sans",
            "source sans",
            "noto sans",
        )
    )


def _derive_typography_keywords(input_data: Mapping[str, Any], derived: list[dict[str, Any]]) -> None:
    families: list[str] = [str(font) for font in input_data.get("fonts") or []]
    typography = input_data.get("typography")
    weight = None
    width = None
    if isinstance(typography, Mapping):
        families.extend(str(font) for font in typography.get("families") or [])
        weight = typography.get("weight")
        width = typography.get("width")
    for family in families:
        lowered = family.lower()
        if "dot" in lowered and "matrix" in lowered:
            _push_derived(derived, "dot_matrix", "typography", 0.8, "font")
        elif any(token in lowered for token in ("mono", "courier", "console", "code")):
            _push_derived(derived, "monospace", "typography", 0.9, "font")
        elif "serif" in lowered and "sans" not in lowered:
            _push_derived(derived, "serif", "typography", 0.85, "font")
        elif "sans" in lowered or _is_known_font(lowered):
            geometric = "geometric" in lowered or any(
                name in lowered
                for name in ("futura", "century gothic", "montserrat", "poppins", "jost", "outfit")
            )
            _push_derived(
                derived,
                "geometric_sans" if geometric else "humanist_sans",
                "typography",
                0.75,
                "font",
            )
        if "condensed" in lowered or "narrow" in lowered:
            _push_derived(derived, "condensed", "typography", 0.7, "font")
        if "expanded" in lowered or "extended" in lowered:
            _push_derived(derived, "expanded", "typography", 0.7, "font")
    if isinstance(weight, (int, float)):
        if weight >= 700:
            _push_derived(derived, "bold", "typography", 0.7, "font")
        elif weight <= 300:
            _push_derived(derived, "light", "typography", 0.65, "font")
    if isinstance(width, str):
        lowered_width = width.lower()
        if lowered_width in {"condensed", "expanded"}:
            _push_derived(derived, lowered_width, "typography", 0.7, "font")


def _density_keyword(density: float) -> str:
    if density <= 0.5:
        return "sparse"
    if density >= 5.0:
        return "dense"
    return "balanced"


def _derive_geometry_keywords(shape: Mapping[str, Any], derived: list[dict[str, Any]]) -> None:
    radius = shape.get("corner_radius_ratio")
    if isinstance(radius, (int, float)):
        if radius >= 0.35:
            _push_derived(derived, "rounded", "form", 0.8, "geometry")
        elif radius <= 0.08:
            _push_derived(derived, "sharp", "form", 0.8, "geometry")
    symmetry = shape.get("symmetry")
    if isinstance(symmetry, str):
        lowered = symmetry.lower()
        if lowered == "radial":
            _push_derived(derived, "circular", "form", 0.8, "geometry")
        elif lowered in {"vertical", "horizontal"}:
            _push_derived(derived, "symmetric", "form", 0.65, "geometry")
    if (shape.get("tick_count") or 0) > 0 or (shape.get("segment_count") or 0) > 0:
        _push_derived(derived, "segmented", "form", 0.6, "geometry")
    density = shape.get("density")
    if isinstance(density, (int, float)):
        _push_derived(derived, _density_keyword(density), "density", 0.7, "geometry")
    texture = shape.get("texture")
    if isinstance(texture, str) and texture.lower() in {
        "flat",
        "noise",
        "scanline",
        "grid",
        "glossy",
        "matte",
    }:
        _push_derived(derived, texture.lower(), "material", 0.65, "geometry")
    elevation = shape.get("elevation")
    if isinstance(elevation, str):
        lowered = elevation.lower()
        if lowered in {"raised", "floating"}:
            _push_derived(derived, "skeuomorphic", "ornament", 0.6, "geometry")
        elif lowered in {"flat", "inset"}:
            _push_derived(derived, "minimal", "ornament", 0.55, "geometry")


def _derive_shape_motion_keywords(shape: Mapping[str, Any], derived: list[dict[str, Any]]) -> None:
    animation = shape.get("animation")
    if not isinstance(animation, str):
        return
    lowered = animation.lower()
    if lowered in {"none", "static"}:
        _push_derived(derived, "static", "motion", 0.8, "motion")
        return
    _push_derived(derived, "animated", "motion", 0.8, "motion")
    if lowered in {"needle", "pulse", "sweep", "shimmer"}:
        _push_derived(derived, lowered, "motion", 0.75, "motion")


def _derive_motion_keywords(motion: Sequence[Any], derived: list[dict[str, Any]]) -> None:
    animated = False
    static_motion = False
    for hint in motion:
        lowered = str(hint).lower()
        if lowered in {"static", "none", "reduced", "reduced_motion"}:
            static_motion = True
        elif lowered in {"animated", "animation"}:
            animated = True
        elif lowered in {"needle", "pulse", "sweep", "shimmer", "glow"}:
            animated = True
            _push_derived(derived, lowered, "motion", 0.7, "motion")
    if animated:
        _push_derived(derived, "animated", "motion", 0.8, "motion")
    elif static_motion:
        _push_derived(derived, "static", "motion", 0.8, "motion")


def _derive_keywords(input_data: Mapping[str, Any]) -> list[dict[str, Any]]:
    derived: list[dict[str, Any]] = []
    _derive_palette_keywords(input_data.get("colors") or [], derived)
    _derive_typography_keywords(input_data, derived)
    shape = input_data.get("shape")
    if isinstance(shape, Mapping):
        _derive_geometry_keywords(shape, derived)
        _derive_shape_motion_keywords(shape, derived)
    _derive_motion_keywords(input_data.get("motion") or [], derived)
    for reference in input_data.get("references") or []:
        keyword = str(reference).strip().lower()
        if keyword:
            _push_derived(derived, keyword, "references", 0.8, "reference")
    derived.sort(key=lambda entry: (entry["field"], entry["keyword"]))
    return derived


def derive_vibe(input_data: Mapping[str, Any]) -> dict[str, Any]:
    """Derive a ``VibeProfile`` from palette, typography, geometry, motion, and references."""
    derived = _derive_keywords(input_data)
    source = input_data.get("source")
    user = input_data.get("user")
    return merge_vibe_keywords(
        source if isinstance(source, Mapping) else None,
        user if isinstance(user, Mapping) else None,
        derived,
    )


# ---------------------------------------------------------------------------
# Coherence scoring
# ---------------------------------------------------------------------------


def _insert_keywords(features: dict[str, set[str]], key: str, values: Iterable[Any]) -> None:
    target = features.setdefault(key, set())
    for value in values:
        text = str(value).strip().lower()
        if text:
            target.add(text)


def _derived_field(entry: Mapping[str, Any]) -> str:
    field = str(entry.get("field") or "")
    return field if field in {"color", "typography", "form", "motion", "density"} else "form"


def _shape_form_keywords(shape: Mapping[str, Any]) -> list[str]:
    keywords: list[str] = []
    radius = shape.get("corner_radius_ratio")
    if isinstance(radius, (int, float)):
        if radius >= 0.35:
            keywords.append("rounded")
        elif radius <= 0.08:
            keywords.append("sharp")
    symmetry = shape.get("symmetry")
    if isinstance(symmetry, str):
        lowered = symmetry.lower()
        if lowered == "radial":
            keywords.append("circular")
        elif lowered in {"vertical", "horizontal"}:
            keywords.append("symmetric")
    if (shape.get("tick_count") or 0) > 0 or (shape.get("segment_count") or 0) > 0:
        keywords.append("segmented")
    return keywords


def _item_feature_map(item: Mapping[str, Any]) -> dict[str, set[str]]:
    features: dict[str, set[str]] = {}
    vibe = item.get("vibe")
    if isinstance(vibe, Mapping):
        for field in ("color", "typography", "form", "motion"):
            _insert_keywords(features, field, vibe.get(field) or [])
        density = vibe.get("density")
        if isinstance(density, str):
            _insert_keywords(features, "density", [density])
        for entry in vibe.get("derived") or []:
            if isinstance(entry, Mapping):
                _insert_keywords(features, _derived_field(entry), [entry.get("keyword")])
    shape = item.get("shape")
    if isinstance(shape, Mapping):
        for keyword in _shape_form_keywords(shape):
            _insert_keywords(features, "form", [keyword])
        density = shape.get("density")
        if isinstance(density, (int, float)):
            _insert_keywords(features, "density", [_density_keyword(float(density))])
        animation = shape.get("animation")
        if isinstance(animation, str):
            lowered = animation.lower()
            if lowered in {"none", "static"}:
                _insert_keywords(features, "motion", ["static"])
            else:
                _insert_keywords(features, "motion", ["animated", lowered])
    return features


def _jaccard(left: set[str], right: set[str]) -> float:
    if not left and not right:
        return 1.0
    union = left | right
    if not union:
        return 1.0
    return len(left & right) / len(union)


def _pairwise_score(sets: Sequence[set[str]]) -> float | None:
    if len(sets) < 2:
        return None
    total = 0.0
    comparisons = 0
    for index, left in enumerate(sets):
        for right in sets[index + 1 :]:
            total += _jaccard(left, right)
            comparisons += 1
    if comparisons == 0:
        return None
    return _round4(total / comparisons)


def _item_similarity(left: Mapping[str, set[str]], right: Mapping[str, set[str]]) -> float:
    total = 0.0
    comparisons = 0
    for key in _DIMENSION_KEYS:
        left_set = left.get(key)
        right_set = right.get(key)
        if not left_set or not right_set:
            continue
        total += _jaccard(left_set, right_set)
        comparisons += 1
    return total / comparisons if comparisons else 0.0


def _item_identifier(items: Sequence[Mapping[str, Any]], index: int) -> str:
    identifier = str(items[index].get("id") or "").strip()
    return identifier or f"item_{index + 1}"


def coherence_for(
    items: Sequence[Mapping[str, Any]],
    options: Mapping[str, Any] | None = None,
) -> dict[str, Any]:
    """Score coherence across widgets, themes, or a scene.

    The report is advisory. A single item returns ``score = 1.0`` with low
    confidence. ``options["intentional_mixing"]`` records an intentional mix.
    """
    options = options or {}
    if not items:
        return {
            "score": 1.0,
            "dimensions": {},
            "outliers": [],
            "conflicts": [],
            "suggestions": [],
            "confidence": 0.0,
        }
    item_features = [_item_feature_map(item) for item in items]
    item_count = len(items)

    dimension_scores: dict[str, float] = {}
    data_counts: dict[str, int] = {}
    conflicts: set[str] = set()
    for key in _DIMENSION_KEYS:
        sets = [features[key] for features in item_features if features.get(key)]
        data_counts[key] = len(sets)
        if item_count == 1 and sets:
            dimension_scores[key] = 1.0
        else:
            score = _pairwise_score(sets)
            if score is not None:
                dimension_scores[key] = score
        flattened = [keyword for feature_set in sets for keyword in feature_set]
        for keyword in flattened:
            for other in flattened:
                if keyword < other and _keywords_conflict(keyword, other):
                    pair = sorted((keyword, other))
                    conflicts.add(f"{pair[0]} vs {pair[1]}")

    total_weight = sum(_DIMENSION_WEIGHTS[key] for key in dimension_scores)
    if item_count == 1 or total_weight <= 0:
        score = 1.0
    else:
        score = _round4(sum(value * _DIMENSION_WEIGHTS[key] for key, value in dimension_scores.items()) / total_weight)

    coverage = sum(data_counts[key] / item_count for key in _DIMENSION_KEYS) / len(_DIMENSION_KEYS)
    confidence = 0.25 if item_count == 1 else _round4(0.5 + 0.5 * coverage)

    outliers: list[str] = []
    if item_count >= 3:
        similarities: list[float] = []
        for index, features in enumerate(item_features):
            best = 0.0
            for other_index, other in enumerate(item_features):
                if index == other_index:
                    continue
                best = max(best, _item_similarity(features, other))
            similarities.append(best)
        best_similarity = max(similarities)
        for index, similarity in enumerate(similarities):
            if similarity < 0.5 and similarity < best_similarity - 0.25:
                outliers.append(_item_identifier(items, index))
        outliers = sorted(set(outliers))

    intentional = bool(options.get("intentional_mixing")) or any(
        "intentional_mixing" in {keyword.lower() for keyword in _profile_keywords(item.get("vibe") or {})}
        for item in items
        if isinstance(item.get("vibe"), Mapping)
    )
    if intentional:
        score = 1.0
        outliers = []
        conflicts = set()
        confidence = max(confidence, 0.25)
    accepted = {str(value) for value in options.get("accepted_conflicts") or []}
    conflicts = {conflict for conflict in conflicts if conflict not in accepted}

    dimensions: dict[str, float] = {}
    for key in ("form", "color", "typography", "density", "motion"):
        if key in dimension_scores:
            dimensions[key] = dimension_scores[key]
    suggestions: list[str] = []
    for key in _DIMENSION_KEYS:
        value = dimension_scores.get(key)
        if value is not None and value < 0.8:
            suggestions.append(f"review {_DIMENSION_LABELS[key]}")
    if intentional:
        suggestions.append("intentional mixing accepted")
    elif conflicts:
        suggestions.append("resolve conflicting keywords")
    suggestions = sorted(set(suggestions))

    return {
        "score": score,
        "dimensions": dimensions,
        "outliers": outliers,
        "conflicts": sorted(conflicts),
        "suggestions": suggestions,
        "confidence": confidence,
    }


__all__ = [
    "MAX_TEXT_INPUT",
    "VIBE_TAXONOMY",
    "ShapeExtractError",
    "coherence_for",
    "derive_vibe",
    "extract_from_css",
    "extract_from_svg",
    "extract_shape",
    "merge_vibe_keywords",
    "parse_svg_path",
]
