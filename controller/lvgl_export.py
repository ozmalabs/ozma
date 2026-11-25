# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Deterministic LVGL C/JSON export for Ozma display layouts.

WGT-064 consumes the same lossless Widget IR / ``ScreenLayout`` projection as
the browser and PNG renderers and emits:

* a C header and source implementing ``lv_obj``, ``lv_arc``, ``lv_bar``,
  ``lv_label``, ``lv_chart``, and ``lv_img`` widgets;
* static threshold and chart arrays plus an init function and per-widget value
  setters for gauges, bars, and numbers;
* JSON metadata including source/IR hashes and an export-only
  :class:`TranslationReport` with explicit approximations.

The exporter does not invoke a compiler or subprocess.  Inputs are treated as
untrusted data: counts, sizes, strings, identifiers, points, and thresholds are
bounded, and malformed documents raise :class:`LvglExportError` instead of
producing partial C.
"""

from __future__ import annotations

import hashlib
import json
import math
import re
from typing import Any, Mapping, Sequence

LVGL_EXPORT_SCHEMA = "org.ozma.lvgl-export/1"
LVGL_TARGET_FORMAT = "lvgl_c"
LVGL_TRANSLATOR = "controller.lvgl_export"
LVGL_TRANSLATOR_VERSION = "1.0.0"

MAX_WIDGETS = 2048
MAX_POINTS = 512
MAX_THRESHOLDS = 64
MAX_DIMENSION = 8192
MAX_TEXT_LENGTH = 2048
MAX_STRING_LENGTH = 4096
MAX_ROWS = 64
MAX_ROW_CELLS = 8
MAX_THEME_NODES = 20_000

WIDGET_KIND_ALIASES = {
    "label": "text",
    "chart": "sparkline",
    "dual-bar": "bar",
    "vu_meter": "bar",
    "meter": "bar",
    "scenario": "status",
    "clock": "text",
    "image": "image",
    "grid": "card",
}

LVGL_TYPES = {
    "gauge": "lv_arc",
    "bar": "lv_bar",
    "number": "lv_label",
    "text": "lv_label",
    "sparkline": "lv_chart",
    "status": "lv_label",
    "icon": "lv_label",
    "image": "lv_img",
    "table": "lv_obj",
    "card": "lv_obj",
    "custom": "lv_obj",
}

DEFAULT_COLORS = {
    "bg": 0x0A0A0F,
    "fg": 0xE5E7EB,
    "muted": 0x8D9BB3,
    "dim": 0x64728A,
    "accent": 0x5B6FFF,
    "ok": 0x42D392,
    "warn": 0xF0B94A,
    "crit": 0xF26464,
    "track": 0x1A1A22,
    "surface": 0x111118,
}

_NAMED_COLORS = {
    "black": 0x000000,
    "white": 0xFFFFFF,
    "red": 0xFF0000,
    "green": 0x00FF00,
    "blue": 0x0000FF,
    "yellow": 0xFFFF00,
    "transparent": 0x000000,
}

_IDENTIFIER_RE = re.compile(r"[^A-Za-z0-9_]")
_HEX_RE = re.compile(r"^#([0-9a-fA-F]{3,8})$")
_RGB_RE = re.compile(
    r"^rgba?\(\s*([0-9]+)\s*,\s*([0-9]+)\s*,\s*([0-9]+)(?:\s*,\s*[0-9.]+\s*)?\)$",
    re.IGNORECASE,
)
_TOKEN_RE = re.compile(r"^\{([^{}]{1,128})\}$")


class LvglExportError(ValueError):
    """A display layout cannot be exported to LVGL."""


# ---------------------------------------------------------------------------
# Small numeric/string helpers
# ---------------------------------------------------------------------------


def _mapping(value: Any, field: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping):
        raise LvglExportError(f"`{field}` must be an object")
    return value


def _finite(value: Any, field: str, fallback: float | None = None) -> float:
    if value is None and fallback is not None:
        return fallback
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise LvglExportError(f"`{field}` must be a finite number")
    number = float(value)
    if not math.isfinite(number):
        raise LvglExportError(f"`{field}` must be a finite number")
    return number


def _round(value: float, places: int = 3) -> float:
    rounded = round(float(value), places)
    return 0.0 if rounded == 0 else rounded


def _clamp(value: float, low: float, high: float) -> float:
    return low if value < low else high if value > high else value


def _text(value: Any, field: str, limit: int = MAX_TEXT_LENGTH) -> str:
    if value is None:
        return ""
    if not isinstance(value, str):
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            value = str(value)
        elif isinstance(value, bool):
            value = "true" if value else "false"
        else:
            raise LvglExportError(f"`{field}` must be a string")
    if len(value) > limit:
        raise LvglExportError(f"`{field}` exceeds {limit} characters")
    return value


def _canonical_json(value: Any) -> str:
    try:
        return json.dumps(
            value,
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
            allow_nan=False,
        )
    except (TypeError, ValueError) as exc:
        raise LvglExportError(f"input is not valid JSON data: {exc}") from exc


def _canonical_hash(value: Any) -> str:
    return hashlib.sha256(_canonical_json(value).encode("utf-8")).hexdigest()


def _format_number(value: float, places: int = 3) -> str:
    text = f"{float(value):.{places}f}".rstrip("0").rstrip(".")
    if text in ("", "-0"):
        return "0"
    return text


def _c_float(value: float) -> str:
    text = _format_number(value)
    if "." not in text:
        text = f"{text}.0"
    return f"{text}f"


def _c_string(value: str) -> str:
    """Return a quoted C string with bounded, byte-safe escaping."""
    try:
        encoded = value.encode("utf-8")
    except UnicodeEncodeError as exc:
        raise LvglExportError("strings must be valid UTF-8") from exc
    output = ['"']
    for byte in encoded:
        if byte == 0x5C:
            output.append("\\\\")
        elif byte == 0x22:
            output.append('\\"')
        elif byte == 0x0A:
            output.append("\\n")
        elif byte == 0x0D:
            output.append("\\r")
        elif byte == 0x09:
            output.append("\\t")
        elif 0x20 <= byte <= 0x7E:
            output.append(chr(byte))
        else:
            # Fixed-width octal escapes cannot absorb a following digit.
            output.append(f"\\{byte:03o}")
    output.append('"')
    return "".join(output)


def _identifier(raw: str, index: int, prefix: str) -> str:
    base = _IDENTIFIER_RE.sub("_", raw).strip("_")[:40]
    if not base:
        base = f"widget_{index}"
    elif base[0].isdigit():
        base = f"w_{base}"
    return f"{prefix}_{base}_{index}"


def _resolve_token(value: Any, tokens: Mapping[str, Any], depth: int = 0) -> Any:
    if not isinstance(value, str) or depth >= 8:
        return value
    match = _TOKEN_RE.match(value.strip())
    if not match:
        return value
    key = match.group(1)
    if key not in tokens:
        return value
    return _resolve_token(tokens[key], tokens, depth + 1)


def _flatten_theme(source: Mapping[str, Any]) -> dict[str, Any]:
    raw: Mapping[str, Any]
    if isinstance(source.get("tokens"), Mapping):
        raw = source["tokens"]  # type: ignore[assignment]
    elif isinstance(source.get("theme"), Mapping) and isinstance(
        source["theme"].get("tokens"), Mapping
    ):
        raw = source["theme"]["tokens"]  # type: ignore[index]
    else:
        raw = source

    flat: dict[str, Any] = {}
    stack: list[tuple[str, Any, int]] = [("", raw, 1)]
    nodes = 0
    while stack:
        prefix, value, depth = stack.pop()
        if depth > 16:
            raise LvglExportError("theme nesting is too deep")
        if not isinstance(value, Mapping):
            continue
        for key, item in value.items():
            nodes += 1
            if nodes > MAX_THEME_NODES:
                raise LvglExportError("theme has too many tokens")
            if key in ("__proto__", "prototype", "constructor"):
                continue
            path = f"{prefix}.{key}" if prefix else str(key)
            if isinstance(item, Mapping):
                if "$value" in item:
                    flat[path] = item["$value"]
                elif "value" in item:
                    flat[path] = item["value"]
                else:
                    stack.append((path, item, depth + 1))
            else:
                flat[path] = item
    return {key: _resolve_token(value, flat) for key, value in flat.items()}


def _parse_color_value(
    value: Any, tokens: Mapping[str, Any] | None = None
) -> int | None:
    """Parse a validated colour without a fallback; ``None`` means invalid."""
    value = _resolve_token(value, tokens or {})
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    lowered = text.lower()
    if lowered in _NAMED_COLORS:
        return _NAMED_COLORS[lowered]
    match = _HEX_RE.match(text)
    if match:
        digits = match.group(1)
        if len(digits) in (3, 4):
            digits = "".join(ch * 2 for ch in digits[:3])
        elif len(digits) in (6, 8):
            digits = digits[:6]
        else:
            return None
        try:
            return int(digits, 16)
        except ValueError:
            return None
    match = _RGB_RE.match(text)
    if match:
        red, green, blue = (int(match.group(i)) for i in (1, 2, 3))
        if any(component > 255 for component in (red, green, blue)):
            return None
        return (red << 16) | (green << 8) | blue
    return None


def _parse_color(value: Any, fallback: int, tokens: Mapping[str, Any] | None = None) -> int:
    parsed = _parse_color_value(value, tokens)
    return fallback if parsed is None else parsed


def _color_hex(value: int) -> str:
    return f"#{value & 0xFFFFFF:06x}"


_NUMBER_TOKEN_RE = re.compile(
    r"^([+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?)(?:px)?$"
)


def _token_number(value: Any, tokens: Mapping[str, Any]) -> float | None:
    """Resolve a finite numeric token; supports unitless numbers and ``Npx``."""
    value = _resolve_token(value, tokens)
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        number = float(value)
    elif isinstance(value, str):
        match = _NUMBER_TOKEN_RE.match(value.strip())
        if not match:
            return None
        try:
            number = float(match.group(1))
        except ValueError:
            return None
    else:
        return None
    return number if math.isfinite(number) else None


def _scope(index: int | None) -> str:
    return "canvas" if index is None else f"widget[{index}]"


def _colour_from_tokens(
    tokens: Mapping[str, Any],
    keys: Sequence[str],
    fallback: int,
    warnings: list[str],
    index: int | None,
    label: str,
) -> tuple[int, bool]:
    """Return ``(colour, used_valid_token)`` with warnings on invalid tokens."""
    for key in keys:
        if key not in tokens:
            continue
        parsed = _parse_color_value(tokens[key], tokens)
        if parsed is None:
            warnings.append(
                f"{_scope(index)} token `{key}` for {label} is not a valid colour; "
                f"using fallback {_color_hex(fallback)}"
            )
            return fallback, False
        return parsed, True
    return fallback, False


def _explicit_colour(
    value: Any,
    tokens: Mapping[str, Any],
    fallback: int,
    warnings: list[str],
    index: int,
    field: str,
) -> tuple[int | None, bool]:
    """Return ``(colour_or_none, valid)`` for an explicit widget colour."""
    if value is None or (isinstance(value, str) and not value.strip()):
        return None, False
    parsed = _parse_color_value(value, tokens)
    if parsed is None:
        warnings.append(
            f"widget[{index}] `{field}` is not a valid colour; "
            f"using fallback {_color_hex(fallback)}"
        )
        return fallback, False
    return parsed, True


def _widget_or_token_colour(
    value: Any,
    tokens: Mapping[str, Any],
    keys: Sequence[str],
    fallback: int,
    warnings: list[str],
    index: int,
    field: str,
    label: str,
) -> int:
    explicit, _valid = _explicit_colour(
        value, tokens, fallback, warnings, index, field
    )
    if explicit is not None:
        return explicit
    parsed, _used = _colour_from_tokens(
        tokens, keys, fallback, warnings, index, label
    )
    return parsed


def _style_int(
    tokens: Mapping[str, Any],
    keys: Sequence[str],
    fallback: int,
    minimum: int,
    maximum: int,
    warnings: list[str],
    index: int,
    label: str,
) -> tuple[int, bool]:
    """Resolve a bounded integer style token; return ``(value, used_token)``."""
    for key in keys:
        if key not in tokens:
            continue
        number = _token_number(tokens[key], tokens)
        if number is None:
            warnings.append(
                f"widget[{index}] token `{key}` for {label} is not a finite "
                f"number; using fallback {fallback}"
            )
            return fallback, False
        rounded = int(_round(number, 0))
        if rounded < minimum or rounded > maximum:
            warnings.append(
                f"widget[{index}] token `{key}` for {label}="
                f"{_format_number(number)} is outside {minimum}..{maximum}; "
                f"using fallback {fallback}"
            )
            return fallback, False
        return rounded, True
    return fallback, False


_TEXT_SIZE_TOKENS = (
    "typography.size",
    "typography.size_px",
    "font.size",
    "font.size_px",
    "text.size",
)


def _text_size(
    widget: Mapping[str, Any],
    tokens: Mapping[str, Any],
    warnings: list[str],
    index: int,
) -> tuple[int | None, bool]:
    """Resolve a text size from widget typography or theme tokens."""
    candidates: list[tuple[str, Any]] = []
    typography = widget.get("typography")
    if isinstance(typography, Mapping) and typography.get("size_px") is not None:
        candidates.append(("typography.size_px", typography["size_px"]))
    if widget.get("font_size") is not None:
        candidates.append(("font_size", widget["font_size"]))
    for key in _TEXT_SIZE_TOKENS:
        if key in tokens:
            candidates.append((f"token `{key}`", tokens[key]))

    for source, value in candidates:
        number = _token_number(value, tokens)
        if number is None:
            warnings.append(
                f"widget[{index}] {source} text size is not a finite number; "
                "ignoring it"
            )
            return None, False
        rounded = int(_round(number, 0))
        if rounded < 4 or rounded > 512:
            warnings.append(
                f"widget[{index}] {source} text size="
                f"{_format_number(number)} is outside 4..512; ignoring it"
            )
            return None, False
        return rounded, True
    return None, False


def _unit_suffix(unit: str) -> str:
    if not unit:
        return ""
    lowered = unit.lower()
    if lowered in ("celsius", "degc"):
        return "°C"
    if lowered in ("fahrenheit", "degf"):
        return "°F"
    if lowered == "percent":
        return "%"
    if lowered in ("%", "°", "°c", "°f"):
        return unit
    return f" {unit}"


def _format_decimal(value: float, decimals: int, unit: str) -> str:
    body = f"{value:.{decimals}f}"
    return f"{body}{_unit_suffix(unit)}"


# ---------------------------------------------------------------------------
# Input extraction and normalisation
# ---------------------------------------------------------------------------


def _extract_layout(value: Any) -> tuple[Mapping[str, Any], list[Any], str]:
    if isinstance(value, (list, tuple)):
        widgets = list(value)
        is_ir = bool(widgets) and all(
            isinstance(item, Mapping)
            and isinstance(item.get("kind"), str)
            and isinstance(item.get("binding"), Mapping)
            for item in widgets
        )
        return (
            {"widgets": widgets},
            widgets,
            "org.ozma.widget-ir" if is_ir else "ozma_screen_layout",
        )
    if not isinstance(value, Mapping):
        raise LvglExportError("layout must be an object or a widget list")

    if isinstance(value.get("layout"), Mapping) and isinstance(
        value["layout"].get("widgets"), list
    ):
        layout = value["layout"]
        widgets = layout["widgets"]
        source_format = str(value.get("source_format") or "ozma_display_definition")
        return layout, list(widgets), source_format
    if isinstance(value.get("widgets"), list):
        return value, list(value["widgets"]), "ozma_screen_layout"
    if isinstance(value.get("ir"), Mapping):
        return {"widgets": [value["ir"]]}, [value["ir"]], "org.ozma.widget-ir"
    if isinstance(value.get("kind"), str) and isinstance(value.get("binding"), Mapping):
        return {"widgets": [value]}, [value], "org.ozma.widget-ir"
    raise LvglExportError("layout must contain a `widgets` array or a Widget IR")


def _widget_kind(widget: Mapping[str, Any]) -> tuple[str, str]:
    raw = widget.get("ir_kind") or widget.get("kind") or widget.get("type") or "card"
    if not isinstance(raw, str):
        raise LvglExportError("widget kind must be a string")
    kind = raw.lower()
    kind = WIDGET_KIND_ALIASES.get(kind, kind)
    return raw, kind if kind in LVGL_TYPES else "card"


def _resolve_widget_min_max(
    widget: Mapping[str, Any], warnings: list[str], index: int
) -> tuple[float, float]:
    domain = widget.get("domain")
    domain = domain if isinstance(domain, Mapping) else {}
    min_value = _finite(widget.get("min", domain.get("min")), f"widgets[{index}].min", 0.0)
    max_value = _finite(widget.get("max", domain.get("max")), f"widgets[{index}].max", 100.0)
    if min_value > max_value:
        warnings.append(f"widget[{index}] had min > max; bounds were swapped")
        min_value, max_value = max_value, min_value
    if min_value == max_value:
        max_value = min_value + 1.0
        warnings.append(f"widget[{index}] had an empty range; max was widened by one")
    return float(min_value), float(max_value)


def _decimals(widget: Mapping[str, Any]) -> int:
    if widget.get("decimals") is not None:
        value = _finite(widget["decimals"], "widgets[].decimals")
        return int(_clamp(value, 0, 8))
    binding = widget.get("binding")
    format_text = widget.get("format")
    if format_text is None and isinstance(binding, Mapping):
        format_text = binding.get("format")
    if isinstance(format_text, str):
        match = re.match(r"^fixed\((\d{1,2})\)$", format_text.strip())
        if match:
            return int(_clamp(float(match.group(1)), 0, 8))
        if format_text.strip() in ("integer", "int"):
            return 0
    return 1


def _threshold_list(
    widget: Mapping[str, Any],
    index: int,
    palette: Mapping[str, int],
    tokens: Mapping[str, Any],
    warnings: list[str],
) -> list[dict[str, Any]]:
    output: list[dict[str, Any]] = []
    domain = widget.get("domain")
    domain = domain if isinstance(domain, Mapping) else {}

    def add(item: Any) -> None:
        if len(output) >= MAX_THRESHOLDS:
            warnings.append(f"widget[{index}] exceeded {MAX_THRESHOLDS} thresholds")
            return
        if not isinstance(item, Mapping):
            return
        at = _finite(item.get("at"), f"widgets[{index}].thresholds.at")
        severity = _text(item.get("severity") or "info", "threshold.severity", 64).lower()
        fallback = palette["accent"]
        if severity in ("warning", "warn"):
            fallback = palette["warn"]
        elif severity in ("critical", "crit", "error"):
            fallback = palette["crit"]
        elif severity in ("ok", "success"):
            fallback = palette["ok"]
        colour = _parse_color(item.get("color"), fallback, tokens)
        output.append(
            {
                "at": _round(at),
                "severity": severity or "info",
                "label": _text(item.get("label"), "threshold.label", 128),
                "color": _color_hex(colour),
                "color_value": colour,
            }
        )

    thresholds = domain.get("thresholds")
    if isinstance(thresholds, list):
        for item in thresholds:
            add(item)
    bands = domain.get("severity_bands")
    if isinstance(bands, list):
        for band in bands:
            if isinstance(band, Mapping) and band.get("min") is not None:
                add(
                    {
                        "at": band.get("min"),
                        "severity": band.get("severity"),
                        "label": band.get("label"),
                    }
                )
    if isinstance(widget.get("thresholds"), list):
        for item in widget["thresholds"]:
            add(item)
    elif isinstance(widget.get("thresholds"), Mapping):
        for severity, at in widget["thresholds"].items():
            add({"at": at, "severity": severity})

    for key, severity, fallback_key in (
        ("warn", "warning", "warn"),
        ("warn_at", "warning", "warn"),
        ("warnAt", "warning", "warn"),
        ("crit", "critical", "crit"),
        ("critical", "critical", "crit"),
        ("crit_at", "critical", "crit"),
        ("critAt", "critical", "crit"),
    ):
        if widget.get(key) is not None:
            explicit_key = "crit_color" if fallback_key == "crit" else "warn_color"
            colour = _parse_color(widget.get(explicit_key), palette[fallback_key], tokens)
            add({"at": widget[key], "severity": severity, "color": _color_hex(colour)})

    output.sort(key=lambda item: (item["at"], item["severity"], item["label"]))
    return output[:MAX_THRESHOLDS]


def _values(widget: Mapping[str, Any], index: int, warnings: list[str]) -> list[float]:
    values = widget.get("values", widget.get("data", widget.get("points")))
    if values is None and isinstance(widget.get("history"), Mapping):
        values = widget["history"].get("values")
    if values is None:
        return []
    if isinstance(values, Mapping):
        values = values.get("values")
    if not isinstance(values, list):
        return []
    output: list[float] = []
    for item in values:
        if len(output) >= MAX_POINTS:
            warnings.append(f"widget[{index}] chart points were bounded to {MAX_POINTS}")
            break
        if isinstance(item, bool) or not isinstance(item, (int, float)):
            continue
        number = float(item)
        if math.isfinite(number):
            output.append(number)
    return output


def _rows(widget: Mapping[str, Any], warnings: list[str], index: int) -> list[str]:
    rows = widget.get("rows", widget.get("data"))
    if not isinstance(rows, list):
        return []
    output: list[str] = []
    for row in rows:
        if len(output) >= MAX_ROWS:
            warnings.append(f"widget[{index}] table rows were bounded to {MAX_ROWS}")
            break
        cells = row if isinstance(row, (list, tuple)) else [row]
        rendered = []
        for cell in cells[:MAX_ROW_CELLS]:
            rendered.append(_text(cell, f"widgets[{index}].rows", 256))
        output.append("    ".join(rendered))
    return output


def _normalise_widget(
    widget: Any,
    index: int,
    tokens: Mapping[str, Any],
    warnings: list[str],
    prefix: str,
) -> dict[str, Any]:
    if not isinstance(widget, Mapping):
        raise LvglExportError(f"widgets[{index}] must be an object")
    raw_kind, kind = _widget_kind(widget)
    if kind == "card" and raw_kind.lower() not in (
        "card",
        "custom",
        "table",
        "grid",
        "unknown",
    ) and raw_kind.lower() not in WIDGET_KIND_ALIASES:
        warnings.append(f'widget "{raw_kind}" exported as a card fallback')

    layout_hint = widget.get("layout")
    layout_hint = layout_hint if isinstance(layout_hint, Mapping) else {}
    x = _finite(widget.get("x", layout_hint.get("x")), f"widgets[{index}].x", 0.0)
    y = _finite(widget.get("y", layout_hint.get("y")), f"widgets[{index}].y", 0.0)
    width = _finite(
        widget.get("w", widget.get("width", layout_hint.get("width"))),
        f"widgets[{index}].w",
        120.0,
    )
    height = _finite(
        widget.get("h", widget.get("height", layout_hint.get("height"))),
        f"widgets[{index}].h",
        120.0,
    )
    if width > MAX_DIMENSION or height > MAX_DIMENSION:
        raise LvglExportError(f"widgets[{index}] exceeds the {MAX_DIMENSION}px size limit")
    width = _clamp(width, 1.0, MAX_DIMENSION)
    height = _clamp(height, 1.0, MAX_DIMENSION)
    x = _clamp(x, -MAX_DIMENSION, MAX_DIMENSION)
    y = _clamp(y, -MAX_DIMENSION, MAX_DIMENSION)

    min_value, max_value = _resolve_widget_min_max(widget, warnings, index)

    arc_limit = max(1, int(min(width, height) // 2))
    arc_default = min(10, arc_limit)
    arc_width = arc_default
    arc_from_token = False
    if kind == "gauge":
        arc_width, arc_from_token = _style_int(
            tokens,
            ("metric.arc_width", "gauge.arc_width"),
            arc_default,
            1,
            arc_limit,
            warnings,
            index,
            "arc width",
        )
    radius_limit = max(0, int(min(width, height) // 2))
    radius_default = min(4, radius_limit)
    bar_radius = radius_default
    bar_radius_from_token = False
    if kind == "bar":
        bar_radius, bar_radius_from_token = _style_int(
            tokens,
            ("metric.bar_radius", "radius.md"),
            radius_default,
            0,
            radius_limit,
            warnings,
            index,
            "bar radius",
        )
    text_size, _text_size_supplied = (
        _text_size(widget, tokens, warnings, index)
        if kind != "image"
        else (None, False)
    )

    surface = DEFAULT_COLORS["surface"]
    surface_supplied = False
    if kind in ("table", "card", "custom"):
        explicit_surface, explicit_surface_valid = _explicit_colour(
            widget.get("surface", widget.get("background", widget.get("bg"))),
            tokens,
            DEFAULT_COLORS["surface"],
            warnings,
            index,
            "surface",
        )
        if explicit_surface is not None:
            surface = explicit_surface
            surface_supplied = explicit_surface_valid
        else:
            surface, surface_token = _colour_from_tokens(
                tokens,
                ("color.surface", "color.background", "canvas.background", "surface"),
                DEFAULT_COLORS["surface"],
                warnings,
                index,
                "surface colour",
            )
            surface_supplied = surface_token

    style_notes: list[str] = []
    if kind == "gauge" and not arc_from_token:
        style_notes.append(
            f"widget[{index}] gauge arc width uses fallback {arc_width}px "
            "(set metric.arc_width or gauge.arc_width)"
        )
    if kind == "bar" and not bar_radius_from_token:
        style_notes.append(
            f"widget[{index}] bar radius uses fallback {bar_radius}px "
            "(set metric.bar_radius or radius.md)"
        )
    if kind in ("table", "card", "custom") and not surface_supplied:
        style_notes.append(
            f"widget[{index}] surface colour uses fallback {_color_hex(surface)} "
            "(set color.surface, color.background, or canvas.background)"
        )
    if text_size is not None:
        style_notes.append(
            f"widget[{index}] text size {text_size}px is retained in metadata; "
            "LVGL C uses the active font"
        )

    palette = {
        "accent": _widget_or_token_colour(
            widget.get("color"),
            tokens,
            ("gauge.fill", "bar.fill", "color.accent", "accent"),
            DEFAULT_COLORS["accent"],
            warnings,
            index,
            "color",
            "accent colour",
        ),
        "track": _widget_or_token_colour(
            widget.get("track", widget.get("track_color")),
            tokens,
            ("gauge.track", "bar.track", "color.track", "track"),
            DEFAULT_COLORS["track"],
            warnings,
            index,
            "track",
            "track colour",
        ),
        "text": _widget_or_token_colour(
            widget.get("text_color"),
            tokens,
            ("gauge.text", "color.fg", "color.text", "text"),
            DEFAULT_COLORS["fg"],
            warnings,
            index,
            "text_color",
            "text colour",
        ),
        "warn": _widget_or_token_colour(
            widget.get("warn_color"),
            tokens,
            ("gauge.warn", "color.warn", "warn"),
            DEFAULT_COLORS["warn"],
            warnings,
            index,
            "warn_color",
            "warning colour",
        ),
        "crit": _widget_or_token_colour(
            widget.get("crit_color"),
            tokens,
            ("gauge.crit", "color.crit", "color.error", "crit"),
            DEFAULT_COLORS["crit"],
            warnings,
            index,
            "crit_color",
            "critical colour",
        ),
        "ok": _widget_or_token_colour(
            widget.get("ok_color"),
            tokens,
            ("color.ok", "ok"),
            DEFAULT_COLORS["ok"],
            warnings,
            index,
            "ok_color",
            "ok colour",
        ),
        "muted": _widget_or_token_colour(
            widget.get("muted_color"),
            tokens,
            ("gauge.muted", "color.muted", "color.dim", "muted"),
            DEFAULT_COLORS["muted"],
            warnings,
            index,
            "muted_color",
            "muted colour",
        ),
    }
    thresholds = _threshold_list(widget, index, palette, tokens, warnings)
    binding = widget.get("binding")
    binding = binding if isinstance(binding, Mapping) else {}
    raw_id = _text(widget.get("ir_id") or widget.get("id"), f"widgets[{index}].id", 256)
    if not raw_id:
        raw_id = f"widget_{index}"
    metric = _text(
        widget.get("metric", binding.get("metric")), f"widgets[{index}].metric", 512
    )
    label = _text(widget.get("label"), f"widgets[{index}].label")
    body_text = _text(
        widget.get("text", widget.get("template")), f"widgets[{index}].text"
    )
    unit = _text(widget.get("unit", binding.get("unit")), f"widgets[{index}].unit", 32)
    decimals = _decimals(widget)

    initial_value = _finite(widget.get("value"), f"widgets[{index}].value", min_value)
    initial_value = _clamp(initial_value, min_value, max_value)
    points = _values(widget, index, warnings)
    if points:
        initial_value = _clamp(points[-1], min_value, max_value)
    rows = _rows(widget, warnings, index)

    return {
        "index": index,
        "id": raw_id,
        "identifier": _identifier(raw_id, index, prefix),
        "raw_kind": raw_kind,
        "kind": kind,
        "target": LVGL_TYPES[kind],
        "x": _round(x),
        "y": _round(y),
        "w": _round(width),
        "h": _round(height),
        "min": _round(min_value),
        "max": _round(max_value),
        "value": _round(initial_value),
        "decimals": decimals,
        "metric": metric,
        "label": label,
        "text": body_text,
        "unit": unit,
        "thresholds": thresholds,
        "points": points,
        "rows": rows,
        "color": palette["accent"],
        "track": palette["track"],
        "text_color": palette["text"],
        "muted": palette["muted"],
        "warn": palette["warn"],
        "crit": palette["crit"],
        "ok": palette["ok"],
        "surface": surface,
        "arc_width": arc_width,
        "bar_radius": bar_radius,
        "text_size": text_size,
        "style_notes": style_notes,
        "source_widget": widget,
    }


# ---------------------------------------------------------------------------
# C generation
# ---------------------------------------------------------------------------


def _threshold_array(record: Mapping[str, Any]) -> list[str]:
    thresholds = record["thresholds"]
    if not thresholds:
        return []
    name = f"{record['identifier']}_thresholds"
    lines = [
        f"static const ozma_lvgl_threshold_t {name}[] = {{",
    ]
    for item in thresholds:
        lines.append(
            f"    {{ {_c_float(item['at'])}, {_c_string(item['severity'])}, {_c_color(item['color_value'])} }},"
        )
    lines.append("};")
    lines.append("")
    return lines


def _c_color(value: int) -> str:
    return f"lv_color_hex(0x{value & 0xFFFFFF:06x})"


def _threshold_colour_block(record: Mapping[str, Any], value_expr: str = "value") -> list[str]:
    thresholds = record["thresholds"]
    if not thresholds:
        return []
    lines = [f"    uint32_t colour = 0x{record['color']:06x};"]
    for item in reversed(thresholds):
        comparison = "if" if item is thresholds[-1] else "else if"
        lines.append(f"    {comparison} ({value_expr} >= {_c_float(item['at'])}) {{")
        lines.append(f"        colour = 0x{item['color_value']:06x};")
        lines.append("    }")
    return lines


def _c_widget_declarations(records: Sequence[Mapping[str, Any]]) -> list[str]:
    lines: list[str] = []
    for record in records:
        ident = record["identifier"]
        lines.append(f"static lv_obj_t *{ident};")
        if record["kind"] in ("gauge", "bar", "number"):
            lines.append(f"static lv_obj_t *{ident}_label;")
        if record["kind"] == "sparkline":
            lines.append(f"static lv_chart_series_t *{ident}_series;")
    if lines:
        lines.append("")
    return lines


def _c_static_arrays(records: Sequence[Mapping[str, Any]]) -> list[str]:
    lines: list[str] = []
    for record in records:
        lines.extend(_threshold_array(record))
        if record["kind"] == "sparkline" and record["points"]:
            ident = record["identifier"]
            values = ", ".join(str(int(round(value))) for value in record["points"])
            lines.append(f"static const lv_coord_t {ident}_points[] = {{ {values} }};")
            lines.append(f"#define {ident.upper()}_POINT_COUNT {len(record['points'])}")
            lines.append("")
        if record["kind"] in ("table", "card") and record["rows"]:
            ident = record["identifier"]
            lines.append(f"static const char *const {ident}_rows[] = {{")
            for row in record["rows"]:
                lines.append(f"    {_c_string(row)},")
            lines.append("};")
            lines.append(f"#define {ident.upper()}_ROW_COUNT {len(record['rows'])}")
            lines.append("")
    return lines


def _format_literal(record: Mapping[str, Any]) -> str:
    unit = _unit_suffix(record["unit"]).replace("%", "%%")
    decimals = record["decimals"]
    return _c_string(f"%.{decimals}f{unit}")


def _c_widget_creation(record: Mapping[str, Any]) -> list[str]:
    ident = record["identifier"]
    kind = record["kind"]
    lines: list[str] = []
    x, y = _format_number(record["x"]), _format_number(record["y"])
    w, h = _format_number(record["w"]), _format_number(record["h"])
    arc_width = _format_number(record["arc_width"])
    bar_radius = _format_number(record["bar_radius"])

    if kind == "gauge":
        lines.extend(
            [
                f"    {ident} = lv_arc_create(root);",
                f"    lv_obj_set_pos({ident}, {x}, {y});",
                f"    lv_obj_set_size({ident}, {w}, {h});",
                f"    lv_arc_set_range({ident}, (int32_t){_c_float(record['min'])}, (int32_t){_c_float(record['max'])});",
                f"    lv_arc_set_bg_angles({ident}, 135, 45);",
                f"    lv_arc_set_value({ident}, (int32_t){_c_float(record['value'])});",
                f"    lv_obj_set_style_arc_width({ident}, {arc_width}, LV_PART_MAIN);",
                f"    lv_obj_set_style_arc_width({ident}, {arc_width}, LV_PART_INDICATOR);",
                f"    lv_obj_set_style_arc_color({ident}, {_c_color(record['track'])}, LV_PART_MAIN);",
                f"    lv_obj_set_style_arc_color({ident}, {_c_color(record['color'])}, LV_PART_INDICATOR);",
                f"    lv_obj_set_style_bg_opa({ident}, LV_OPA_TRANSP, LV_PART_MAIN);",
                f"    {ident}_label = lv_label_create({ident});",
                f"    lv_label_set_text({ident}_label, {_c_string(_format_decimal(record['value'], record['decimals'], record['unit']))});",
                f"    lv_obj_align({ident}_label, LV_ALIGN_CENTER, 0, 0);",
                f"    lv_obj_set_style_text_color({ident}_label, {_c_color(record['text_color'])}, LV_PART_MAIN);",
            ]
        )
    elif kind == "bar":
        lines.extend(
            [
                f"    {ident} = lv_bar_create(root);",
                f"    lv_obj_set_pos({ident}, {x}, {y});",
                f"    lv_obj_set_size({ident}, {w}, {h});",
                f"    lv_bar_set_range({ident}, (int32_t){_c_float(record['min'])}, (int32_t){_c_float(record['max'])});",
                f"    lv_bar_set_value({ident}, (int32_t){_c_float(record['value'])}, LV_ANIM_OFF);",
                f"    lv_obj_set_style_bg_color({ident}, {_c_color(record['track'])}, LV_PART_MAIN);",
                f"    lv_obj_set_style_bg_color({ident}, {_c_color(record['color'])}, LV_PART_INDICATOR);",
                f"    lv_obj_set_style_radius({ident}, {bar_radius}, LV_PART_MAIN);",
                f"    {ident}_label = lv_label_create(root);",
                f"    lv_label_set_text({ident}_label, {_c_string(_format_decimal(record['value'], record['decimals'], record['unit']))});",
                f"    lv_obj_set_pos({ident}_label, {x}, {y});",
                f"    lv_obj_set_style_text_color({ident}_label, {_c_color(record['text_color'])}, LV_PART_MAIN);",
            ]
        )
    elif kind in ("number", "text", "status", "icon"):
        if kind == "text":
            text = record["text"] or record["label"]
        elif kind == "icon":
            icon = record["source_widget"].get("icon")
            if isinstance(icon, Mapping):
                icon = icon.get("name") or icon.get("asset_id")
            text = record["label"] or _text(icon, "icon", 128) or record["id"]
        elif kind == "status":
            text = record["label"] or _format_decimal(
                record["value"], record["decimals"], record["unit"]
            )
        else:
            text = _format_decimal(record["value"], record["decimals"], record["unit"])
        lines.extend(
            [
                f"    {ident} = lv_label_create(root);",
                f"    lv_obj_set_pos({ident}, {x}, {y});",
                f"    lv_obj_set_size({ident}, {w}, {h});",
                f"    lv_label_set_text({ident}, {_c_string(text)});",
                f"    lv_label_set_long_mode({ident}, LV_LABEL_LONG_WRAP);",
                f"    lv_obj_set_style_text_color({ident}, {_c_color(record['text_color'])}, LV_PART_MAIN);",
            ]
        )
        if kind == "number":
            lines.append(f"    {ident}_label = {ident};")
        if kind == "status":
            lines.extend(
                [
                    f"    lv_obj_t *{ident}_dot = lv_obj_create(root);",
                    f"    lv_obj_set_size({ident}_dot, 12, 12);",
                    f"    lv_obj_set_pos({ident}_dot, {x}, {y});",
                    f"    lv_obj_set_style_radius({ident}_dot, LV_RADIUS_CIRCLE, LV_PART_MAIN);",
                    f"    lv_obj_set_style_bg_color({ident}_dot, {_c_color(record['ok'])}, LV_PART_MAIN);",
                    f"    lv_obj_set_style_border_width({ident}_dot, 0, LV_PART_MAIN);",
                ]
            )
    elif kind == "sparkline":
        point_count = max(1, len(record["points"]))
        lines.extend(
            [
                f"    {ident} = lv_chart_create(root);",
                f"    lv_obj_set_pos({ident}, {x}, {y});",
                f"    lv_obj_set_size({ident}, {w}, {h});",
                f"    lv_chart_set_type({ident}, LV_CHART_TYPE_LINE);",
                f"    lv_chart_set_point_count({ident}, {point_count});",
                f"    lv_chart_set_range({ident}, LV_CHART_AXIS_PRIMARY_Y, (int32_t){_c_float(record['min'])}, (int32_t){_c_float(record['max'])});",
                f"    lv_chart_set_div_line_count({ident}, 0, 0);",
                f"    {ident}_series = lv_chart_add_series({ident}, {_c_color(record['color'])}, LV_CHART_AXIS_PRIMARY_Y);",
            ]
        )
        if record["points"]:
            lines.extend(
                [
                    f"    for (uint32_t i = 0; i < {ident.upper()}_POINT_COUNT; ++i) {{",
                    f"        lv_chart_set_next_value({ident}, {ident}_series, {ident}_points[i]);",
                    "    }",
                ]
            )
    elif kind == "image":
        src = _text(
            record["source_widget"].get("url")
            or record["source_widget"].get("image_url")
            or record["source_widget"].get("src"),
            "image.src",
            MAX_STRING_LENGTH,
        )
        lines.extend(
            [
                f"    {ident} = lv_img_create(root);",
                f"    lv_obj_set_pos({ident}, {x}, {y});",
                f"    lv_obj_set_size({ident}, {w}, {h});",
                f"    lv_img_set_src({ident}, {_c_string(src or record['label'] or record['id'])});",
            ]
        )
    else:  # table / card / custom fallback
        lines.extend(
            [
                f"    {ident} = lv_obj_create(root);",
                f"    lv_obj_set_pos({ident}, {x}, {y});",
                f"    lv_obj_set_size({ident}, {w}, {h});",
                f"    lv_obj_set_style_bg_color({ident}, {_c_color(record['surface'])}, LV_PART_MAIN);",
                f"    lv_obj_set_style_radius({ident}, 4, LV_PART_MAIN);",
            ]
        )
        if record["label"]:
            lines.extend(
                [
                    f"    lv_obj_t *{ident}_title = lv_label_create({ident});",
                    f"    lv_label_set_text({ident}_title, {_c_string(record['label'])});",
                    f"    lv_obj_set_pos({ident}_title, 4, 4);",
                    f"    lv_obj_set_style_text_color({ident}_title, {_c_color(record['text_color'])}, LV_PART_MAIN);",
                ]
            )
        if record["rows"]:
            lines.extend(
                [
                    f"    for (uint32_t i = 0; i < {ident.upper()}_ROW_COUNT; ++i) {{",
                    f"        lv_obj_t *row = lv_label_create({ident});",
                    f"        lv_label_set_text(row, {ident}_rows[i]);",
                    "        lv_obj_set_pos(row, 4, 24 + (int32_t)(i * 16));",
                    f"        lv_obj_set_style_text_color(row, {_c_color(record['muted'])}, LV_PART_MAIN);",
                    "    }",
                ]
            )
    lines.append("")
    return lines


def _c_setters(records: Sequence[Mapping[str, Any]]) -> list[str]:
    lines: list[str] = []
    for record in records:
        if record["kind"] not in ("gauge", "bar", "number"):
            continue
        ident = record["identifier"]
        kind = record["kind"]
        lines.append(f"void {ident}_set_value(float value)")
        lines.append("{")
        lines.append(f"    if (value < {_c_float(record['min'])}) value = {_c_float(record['min'])};")
        lines.append(f"    if (value > {_c_float(record['max'])}) value = {_c_float(record['max'])};")
        if kind == "gauge":
            lines.append(f"    lv_arc_set_value({ident}, (int32_t)value);")
        elif kind == "bar":
            lines.append(f"    lv_bar_set_value({ident}, (int32_t)value, LV_ANIM_OFF);")
        lines.extend(_threshold_colour_block(record))
        if record["thresholds"]:
            target = "LV_PART_INDICATOR" if kind in ("gauge", "bar") else "LV_PART_MAIN"
            if kind == "gauge":
                lines.append(
                    f"    lv_obj_set_style_arc_color({ident}, lv_color_hex(colour), {target});"
                )
            elif kind == "bar":
                lines.append(
                    f"    lv_obj_set_style_bg_color({ident}, lv_color_hex(colour), {target});"
                )
        lines.append(f"    lv_label_set_text_fmt({ident}_label, {_format_literal(record)}, (double)value);")
        lines.append("}")
        lines.append("")
    return lines


def _generate_header(records: Sequence[Mapping[str, Any]], prefix: str) -> str:
    guard = f"{prefix.upper()}_EXPORT_H"
    lines = [
        "/* Generated by Ozma controller.lvgl_export; do not edit. */",
        f"#ifndef {guard}",
        f"#define {guard}",
        "",
        '#include "lvgl.h"',
        "#include <stdint.h>",
        "",
        "#ifdef __cplusplus",
        'extern "C" {',
        "#endif",
        "",
        "typedef struct {",
        "    float at;",
        "    const char *severity;",
        "    lv_color_t color;",
        "} ozma_lvgl_threshold_t;",
        "",
        f"#define {prefix.upper()}_WIDGET_COUNT {len(records)}",
        "",
        f"void {prefix}_init(lv_obj_t *parent);",
    ]
    for record in records:
        if record["kind"] in ("gauge", "bar", "number"):
            lines.append(f"void {record['identifier']}_set_value(float value);")
    lines.extend(
        [
            "",
            "#ifdef __cplusplus",
            "}",
            "#endif",
            "",
            f"#endif /* {guard} */",
            "",
        ]
    )
    return "\n".join(lines)


def _generate_source(
    records: Sequence[Mapping[str, Any]],
    width: int,
    height: int,
    background: int,
    prefix: str,
    source_hash: str,
) -> str:
    lines = [
        "/* Generated by Ozma controller.lvgl_export; do not edit. */",
        f"/* source_hash: {source_hash} */",
        f'#include "{prefix}_export.h"',
        "",
        f"#define {prefix.upper()}_WIDTH {width}",
        f"#define {prefix.upper()}_HEIGHT {height}",
        f"#define {prefix.upper()}_BG 0x{background:06x}",
        "",
    ]
    lines.extend(_c_widget_declarations(records))
    lines.extend(_c_static_arrays(records))
    lines.extend(_c_setters(records))
    lines.append(f"void {prefix}_init(lv_obj_t *parent)")
    lines.append("{")
    lines.append("    lv_obj_t *root = lv_obj_create(parent);")
    lines.append("    lv_obj_set_pos(root, 0, 0);")
    lines.append(f"    lv_obj_set_size(root, {prefix.upper()}_WIDTH, {prefix.upper()}_HEIGHT);")
    lines.append(
        f"    lv_obj_set_style_bg_color(root, lv_color_hex({prefix.upper()}_BG), LV_PART_MAIN);"
    )
    lines.append("    lv_obj_set_style_bg_opa(root, LV_OPA_COVER, LV_PART_MAIN);")
    lines.append("    lv_obj_set_style_border_width(root, 0, LV_PART_MAIN);")
    lines.append("    lv_obj_clear_flag(root, LV_OBJ_FLAG_SCROLLABLE);")
    lines.append("")
    for record in records:
        lines.extend(_c_widget_creation(record))
    lines.append("}")
    lines.append("")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Report and metadata
# ---------------------------------------------------------------------------


def _build_report(
    source_format: str,
    records: Sequence[Mapping[str, Any]],
    approximations: list[str],
    dropped: list[str],
    warnings: list[str],
) -> dict[str, Any]:
    converted = [
        f"widget[{record['index']}] {record['kind']} -> {record['target']}"
        for record in records
    ]
    supported = all(
        record["kind"]
        in ("gauge", "bar", "number", "text", "sparkline", "status", "icon", "image")
        for record in records
    )
    fidelity = "semantic" if supported and not warnings else "approximate"
    report = {
        "source_format": source_format,
        "target_format": LVGL_TARGET_FORMAT,
        "translator": LVGL_TRANSLATOR,
        "translator_version": LVGL_TRANSLATOR_VERSION,
        "direction": "export",
        "fidelity": fidelity,
        "converted": converted,
        "approximated": list(approximations),
        "dropped": list(dropped),
        "warnings": list(warnings),
    }
    # Shape checks mirror controller.display_ir.validate_translation_report.
    if report["direction"] != "export":
        raise LvglExportError("translation report direction must be `export`")
    if report["fidelity"] not in ("exact", "semantic", "approximate", "visual_only"):
        raise LvglExportError("invalid translation fidelity")
    return report


def _collect_notes(
    records: Sequence[Mapping[str, Any]], warnings: list[str], background_note: bool
) -> tuple[list[str], list[str]]:
    approximations: list[str] = []
    dropped: list[str] = []
    for record in records:
        approximations.extend(record.get("style_notes", []))
        kind = record["kind"]
        if kind == "gauge":
            approximations.append(
                f"widget[{record['index']}] gauge tick marks and needle are represented by lv_arc"
            )
            dropped.append(f"widget[{record['index']}] gauge animation/effects")
        if kind == "sparkline" and record["points"]:
            approximations.append(
                f"widget[{record['index']}] sparkline history is bounded to {len(record['points'])} points"
            )
        if kind == "text":
            approximations.append(
                f"widget[{record['index']}] text placeholders remain literal until runtime binding"
            )
        if kind in ("table", "card", "custom"):
            approximations.append(
                f"widget[{record['index']}] {kind} is flattened to LVGL labels"
            )
        if kind == "image":
            approximations.append(
                f"widget[{record['index']}] image asset is referenced by path, not embedded"
            )
        if record["thresholds"]:
            approximations.append(
                f"widget[{record['index']}] threshold colours are applied by the generated setter"
            )
        if record["source_widget"].get("actions"):
            dropped.append(f"widget[{record['index']}] actions/interaction")
        if record["source_widget"].get("accessibility"):
            approximations.append(
                f"widget[{record['index']}] accessibility intent is not representable in LVGL C"
            )
    if background_note:
        approximations.append("RGBA alpha channels are flattened to RGB565-compatible colours")
    approximations.extend(warnings)
    return approximations, dropped


def _build_export(
    widgets_or_layout: Any,
    options: Mapping[str, Any] | None,
    *,
    want_source: bool,
) -> dict[str, Any]:
    options = {} if options is None else _mapping(options, "options")
    layout, widgets, source_format = _extract_layout(widgets_or_layout)
    if not widgets:
        raise LvglExportError("layout must contain at least one widget")
    if len(widgets) > MAX_WIDGETS:
        raise LvglExportError(f"layout exceeds {MAX_WIDGETS} widgets")

    width_value = _finite(options.get("width", layout.get("width", 480)), "width", 480)
    height_value = _finite(options.get("height", layout.get("height", 480)), "height", 480)
    if width_value > MAX_DIMENSION or height_value > MAX_DIMENSION:
        raise LvglExportError(f"canvas exceeds the {MAX_DIMENSION}px size limit")
    width = int(_clamp(width_value, 1, MAX_DIMENSION))
    height = int(_clamp(height_value, 1, MAX_DIMENSION))

    theme_source = options.get("tokens") or options.get("theme") or {}
    tokens = _flatten_theme(theme_source) if isinstance(theme_source, Mapping) else {}
    warnings: list[str] = []
    canvas_notes: list[str] = []
    background_value = options.get("background", layout.get("background"))
    if background_value is not None:
        parsed_background = _parse_color_value(background_value, tokens)
        if parsed_background is None:
            warnings.append(
                "canvas background is not a valid colour; "
                f"using fallback {_color_hex(DEFAULT_COLORS['bg'])}"
            )
            background = DEFAULT_COLORS["bg"]
        else:
            background = parsed_background
    else:
        background, background_from_token = _colour_from_tokens(
            tokens,
            ("canvas.background", "color.background", "color.surface", "background"),
            DEFAULT_COLORS["bg"],
            warnings,
            None,
            "canvas background",
        )
        if not background_from_token:
            canvas_notes.append(
                f"canvas background uses fallback {_color_hex(background)} "
                "(set canvas.background, color.background, or color.surface)"
            )
    prefix_raw = options.get("function_prefix", "ozma_lvgl")
    prefix = _IDENTIFIER_RE.sub("_", str(prefix_raw)).strip("_")[:32] or "ozma_lvgl"
    if prefix[0].isdigit():
        prefix = f"ozma_{prefix}"

    records = [
        _normalise_widget(widget, index, tokens, warnings, prefix)
        for index, widget in enumerate(widgets)
    ]

    rgba_seen = any(
        isinstance(value, str) and "rgba(" in value.lower() for value in tokens.values()
    ) or (
        isinstance(layout.get("background"), str)
        and "rgba(" in layout["background"].lower()
    )
    approximations, dropped = _collect_notes(
        records,
        warnings,
        background_note=rgba_seen,
    )
    approximations = [*canvas_notes, *approximations]

    source_hash = _canonical_hash(widgets_or_layout)
    metadata_widgets = []
    for record in records:
        metadata_widgets.append(
            {
                "index": record["index"],
                "id": record["id"],
                "identifier": record["identifier"],
                "kind": record["kind"],
                "target": record["target"],
                "x": record["x"],
                "y": record["y"],
                "w": record["w"],
                "h": record["h"],
                "min": record["min"],
                "max": record["max"],
                "value": record["value"],
                "decimals": record["decimals"],
                "metric": record["metric"],
                "label": record["label"],
                "unit": record["unit"],
                "color": _color_hex(record["color"]),
                "track_color": _color_hex(record["track"]),
                "text_color": _color_hex(record["text_color"]),
                "muted_color": _color_hex(record["muted"]),
                "surface": _color_hex(record["surface"]),
                "warn_color": _color_hex(record["warn"]),
                "crit_color": _color_hex(record["crit"]),
                "arc_width": record["arc_width"],
                "bar_radius": record["bar_radius"],
                "text_size": record["text_size"],
                "thresholds": [
                    {
                        "at": item["at"],
                        "severity": item["severity"],
                        "color": item["color"],
                    }
                    for item in record["thresholds"]
                ],
                "points": len(record["points"]),
                "rows": len(record["rows"]),
            }
        )
    ir_hash = _canonical_hash(metadata_widgets)
    report = _build_report(
        source_format, records, approximations, dropped, warnings
    )

    metadata = {
        "schema": LVGL_EXPORT_SCHEMA,
        "target": "lvgl",
        "target_format": LVGL_TARGET_FORMAT,
        "version": LVGL_TRANSLATOR_VERSION,
        "canvas": {
            "width": width,
            "height": height,
            "background": _color_hex(background),
        },
        "source_format": source_format,
        "source_hash": source_hash,
        "ir_hash": ir_hash,
        "counts": {
            "widgets": len(records),
            "thresholds": sum(len(record["thresholds"]) for record in records),
            "points": sum(len(record["points"]) for record in records),
        },
        "widgets": metadata_widgets,
        "translation_report": report,
    }

    if not want_source:
        return {"metadata": metadata}

    header = _generate_header(records, prefix)
    source = _generate_source(
        records,
        width,
        height,
        background,
        prefix,
        source_hash,
    )
    return {"source": source, "header": header, "metadata": metadata}


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def export_lvgl(
    widgets_or_layout: Any, options: Mapping[str, Any] | None = None
) -> dict[str, Any]:
    """Export widgets/layout to deterministic LVGL C source, header, metadata.

    Returns a dict with ``source``, ``header``, and ``metadata`` keys.  The
    metadata includes ``source_hash``, ``ir_hash``, and a
    ``translation_report`` whose ``direction`` is ``export``.
    """
    return _build_export(widgets_or_layout, options, want_source=True)


def export_lvgl_json(
    widgets_or_layout: Any, options: Mapping[str, Any] | None = None
) -> dict[str, Any]:
    """Return the export tooling metadata (including the translation report)."""
    return _build_export(widgets_or_layout, options, want_source=False)["metadata"]


__all__ = [
    "LVGL_EXPORT_SCHEMA",
    "LVGL_TARGET_FORMAT",
    "LVGL_TRANSLATOR",
    "LVGL_TRANSLATOR_VERSION",
    "LvglExportError",
    "MAX_DIMENSION",
    "MAX_WIDGETS",
    "export_lvgl",
    "export_lvgl_json",
]
