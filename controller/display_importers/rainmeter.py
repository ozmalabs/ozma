# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless Rainmeter ``.ini`` importer.

Rainmeter skins are flat INI documents whose measures supply values and whose
meters describe how those values are drawn. This importer keeps every section
and key (including duplicates), resolves the common meter/measure families to
:class:`WidgetIR`, and retains scripts, plugins, and local asset references as
opaque, non-executable references. Local files are never read.
"""

from __future__ import annotations

import configparser
import math
import re
from typing import Any, Mapping

from .base import (
    BaseImporter,
    ImportAsset,
    ImportContext,
    ImportError,
    ImportResult,
    make_source_capture,
    make_source_document,
    make_widget,
)

_METER_KINDS = {
    "string": "text",
    "bar": "bar",
    "line": "sparkline",
    "histogram": "chart",
    "roundline": "gauge",
    "rotator": "gauge",
    "image": "icon",
    "button": "status",
    "shape": "custom",
}

_MEASURE_TEXT_TYPES = {"string", "time", "uptime"}

_ASSET_KEYS = {
    "imagename",
    "barimage",
    "lineimage",
    "buttonimage",
    "imagepath",
    "path",
    "file",
    "scriptfile",
    "fontfile",
    "maskimage",
    "imagetint",
}

_SCRIPT_KEYS = {"scriptfile", "lua", "script"}

_KNOWN_METER_KEYS = {
    "meter",
    "measurename",
    "measurename2",
    "measurename3",
    "minvalue",
    "maxvalue",
    "x",
    "y",
    "w",
    "h",
    "text",
    "fontface",
    "fontsize",
    "fontweight",
    "fontcolor",
    "solidcolor",
    "solidcolor1",
    "solidcolor2",
    "barcolor",
    "linecolor",
    "histogramcolor",
    "imagename",
    "barimage",
    "lineimage",
    "buttonimage",
    "imagepath",
    "path",
    "file",
    "tooltip",
    "antialias",
    "percentual",
    "numofdecimals",
    "autoscalemax",
    "gradientangle",
    "barorientation",
    "flip",
    "transformationmatrix",
    "dynamicvariables",
    "group",
    "hidden",
    "container",
    "preserveaspectratio",
}

_KNOWN_MEASURE_KEYS = {
    "measure",
    "minvalue",
    "maxvalue",
    "unit",
    "name",
    "format",
    "regkey",
    "reghkey",
    "regvalue",
    "value",
    "formula",
    "scriptfile",
    "plugin",
    "disabled",
    "group",
    "updatedivider",
    "averagesize",
    "string",
    "errorstring",
}

_SECTION_RE = re.compile(r"^\s*\[(?P<name>[^\]]+)\]\s*$")


def _number(value: Any) -> float | None:
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        number = float(value)
    else:
        text = str(value).strip()
        if not text:
            return None
        try:
            number = float(text)
        except ValueError:
            return None
    return number if math.isfinite(number) else None


def _color(value: Any) -> str | None:
    if value is None:
        return None
    text = str(value).strip()
    if not text:
        return None
    if re.fullmatch(r"#[0-9a-fA-F]{6}([0-9a-fA-F]{2})?", text):
        return text.lower()
    parts = [part.strip() for part in text.split(",")]
    if len(parts) in {3, 4} and all(part.isdigit() for part in parts):
        channels = [int(part) for part in parts]
        if all(0 <= channel <= 255 for channel in channels):
            red, green, blue = channels[:3]
            return f"#{red:02x}{green:02x}{blue:02x}"
    return None


def _slug(value: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-")
    return slug or "item"


def _get(items: Mapping[str, str], key: str) -> str | None:
    target = key.lower()
    for name, value in items.items():
        if str(name).lower() == target:
            return value
    return None


def _unknown_keys(items: Mapping[str, str], known: set[str]) -> list[str]:
    known_lower = {key.lower() for key in known}
    return [str(key) for key in items if str(key).lower() not in known_lower]


class RainmeterImporter(BaseImporter):
    """Import Rainmeter skin ``.ini`` documents losslessly."""

    format = "rainmeter_ini"
    version = "1"
    suffixes = (".ini", ".inc")
    mime = "text/plain"

    def sniff(self, data: bytes) -> bool:
        if not data:
            return False
        text = data[:8192].decode("utf-8", errors="ignore")
        return re.search(r"(?im)^\s*\[\s*(?:measure|meter)", text) is not None

    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        raw = bytes(data)
        text = raw.decode("utf-8-sig", errors="replace")
        source_path = name or "skin.ini"
        original = ImportAsset(
            path=source_path, data=raw, kind="original", executable=False, sandbox="none"
        )
        assets = [original]
        warnings: list[str] = []
        unknown: dict[str, Any] = {
            "sections": {},
            "raw_sections": {},
            "scripts": [],
            "asset_references": [],
        }

        parser = configparser.ConfigParser(
            strict=False,
            interpolation=None,
            delimiters=("=",),
        )
        parser.optionxform = str
        try:
            parser.read_string(text, source=source_path)
        except configparser.Error as exc:
            raise ImportError(f"invalid Rainmeter INI: {exc}") from exc

        raw_sections = self._scan_raw_sections(text)
        section_order = parser.sections()
        sections: dict[str, dict[str, str]] = {}
        for section in section_order:
            try:
                items = dict(parser.items(section, raw=True))
            except configparser.Error as exc:  # pragma: no cover - defensive
                warnings.append(f"section `{section}` could not be read: {exc}")
                items = {}
            sections[section] = items
            unknown["sections"][section] = items
            unknown["raw_sections"][section] = raw_sections.get(section, [])

        measures = {
            section: items
            for section, items in sections.items()
            if section.lower().startswith("measure")
        }
        measure_lookup = {
            section.lower(): items for section, items in measures.items()
        }
        meters = {
            section: items
            for section, items in sections.items()
            if section.lower().startswith("meter")
        }
        referenced_measures: set[str] = set()
        for items in meters.values():
            for key in ("MeasureName", "MeasureName2", "MeasureName3"):
                value = _get(items, key)
                if not value:
                    continue
                for part in re.split(r"[|,]", value):
                    name_part = part.strip()
                    if name_part:
                        referenced_measures.add(name_part.lower())

        widgets: list[dict[str, Any]] = []
        seen_ids: set[str] = set()
        asset_references: list[dict[str, str]] = []
        for section, items in sections.items():
            for key, value in items.items():
                if str(key).lower() in _ASSET_KEYS:
                    asset_references.append(
                        {"section": section, "key": str(key), "value": value}
                    )

        for section, items in measures.items():
            measure_type = (_get(items, "Measure") or "").lower()
            script_file = _get(items, "ScriptFile")
            if measure_type == "script" or script_file:
                unknown["scripts"].append(
                    {
                        "section": section,
                        "measure_type": measure_type or "script",
                        "script_file": script_file or _get(items, "Lua") or "",
                        "config": items,
                        "executable": False,
                        "executed": False,
                    }
                )
            if section.lower() in referenced_measures:
                continue
            kind = "text" if measure_type in _MEASURE_TEXT_TYPES else "number"
            widget = self._build_widget(
                raw=raw,
                source_format=self.format,
                section=section,
                items=items,
                kind=kind,
                metric=section,
                label=section,
                min_value=_number(_get(items, "MinValue")),
                max_value=_number(_get(items, "MaxValue")),
                layout={},
                style=None,
                typography=None,
                unknown_keys=_unknown_keys(items, _KNOWN_MEASURE_KEYS),
                asset_references=[ref for ref in asset_references if ref["section"] == section],
                seen_ids=seen_ids,
                suffix="measure",
                unit=_get(items, "Unit") or "",
            )
            widgets.append(widget)

        for section, items in meters.items():
            meter_type = (_get(items, "Meter") or "").lower()
            kind = _METER_KINDS.get(meter_type)
            if kind is None:
                warnings.append(
                    f"meter `{section}` has unsupported type `{meter_type or 'unknown'}`;"
                    " retained opaque"
                )
                kind = "custom"
            measure_names = []
            for key in ("MeasureName", "MeasureName2", "MeasureName3"):
                value = _get(items, key)
                if value:
                    measure_names.extend(
                        part.strip() for part in re.split(r"[|,]", value) if part.strip()
                    )
            metric = measure_names[0] if measure_names else section
            source_measure = measure_lookup.get(metric.lower(), {})
            unit = _get(items, "Unit") or _get(source_measure, "Unit")

            minimum = _number(_get(items, "MinValue"))
            maximum = _number(_get(items, "MaxValue"))
            if minimum is None:
                minimum = _number(_get(source_measure, "MinValue"))
            if maximum is None:
                maximum = _number(_get(source_measure, "MaxValue"))
            if minimum is not None and maximum is not None and minimum > maximum:
                warnings.append(
                    f"meter `{section}` has min greater than max; bounds retained opaque"
                )
                minimum = maximum = None

            layout: dict[str, Any] = {}
            for key, names in (
                ("x", ("X",)),
                ("y", ("Y",)),
                ("width", ("W", "Width")),
                ("height", ("H", "Height")),
            ):
                number = _number(_get(items, names[0]))
                if number is None:
                    continue
                if key in {"width", "height"} and number <= 0:
                    continue
                layout[key] = number

            tokens: dict[str, Any] = {}
            for key in (
                "FontColor",
                "SolidColor",
                "BarColor",
                "LineColor",
                "HistogramColor",
            ):
                value = _get(items, key)
                colour = _color(value)
                if colour:
                    tokens[key.lower()] = colour
            font_face = _get(items, "FontFace")
            if font_face:
                tokens["font_family"] = font_face
            style = {"tokens": tokens} if tokens else None

            typography: dict[str, Any] = {}
            font_size = _number(_get(items, "FontSize"))
            if font_size is not None and font_size > 0:
                typography["size_px"] = font_size
            weight = _number(_get(items, "FontWeight"))
            if weight is not None:
                typography["weight"] = max(1, min(1000, int(weight)))
            if typography:
                typography["intent"] = (
                    "numeric" if kind in {"number", "bar", "gauge"} else "label"
                )

            label = _get(items, "Text") or section
            widget = self._build_widget(
                raw=raw,
                source_format=self.format,
                section=section,
                items=items,
                kind=kind,
                metric=metric,
                label=label,
                min_value=minimum,
                max_value=maximum,
                layout=layout,
                style=style,
                typography=typography or None,
                unknown_keys=_unknown_keys(items, _KNOWN_METER_KEYS),
                asset_references=[ref for ref in asset_references if ref["section"] == section],
                seen_ids=seen_ids,
                suffix="meter",
                measure_names=measure_names,
                unit=unit or "",
            )
            widgets.append(widget)

        unknown["asset_references"] = asset_references
        if not widgets:
            warnings.append("no Rainmeter measures or meters were mapped; INI retained")
        for script in unknown["scripts"]:
            unknown.setdefault("opaque", []).append(f"script:{script['section']}")
        for ref in asset_references:
            unknown.setdefault("opaque", []).append(
                f"asset:{ref['section']}:{ref['key']}"
            )

        parsed = {
            "sections": sections,
            "raw_sections": raw_sections,
            "section_order": section_order,
            "raw_text": text,
        }
        source_document = make_source_document(
            self.format,
            self.version,
            raw,
            original=original,
            parsed=parsed,
            unknown=unknown,
        )
        source_capture = make_source_capture(
            self.format,
            self.version,
            raw,
            assets=assets,
            opaque=list(dict.fromkeys(unknown.get("opaque", []))),
            warnings=warnings,
        )
        extensions = {
            "rainmeter": {
                "section_count": len(section_order),
                "measure_count": len(measures),
                "meter_count": len(meters),
                "script_count": len(unknown["scripts"]),
                "asset_reference_count": len(asset_references),
                "sections": sections,
                "raw_sections": raw_sections,
                "scripts": unknown["scripts"],
                "asset_references": asset_references,
                "opaque": unknown.get("opaque", []),
                "raw_text": text,
            }
        }
        return ImportResult(
            format=self.format,
            version=self.version,
            raw=raw,
            widgets=widgets,
            source_document=source_document,
            source_capture=source_capture,
            assets=assets,
            unknown=unknown,
            warnings=warnings,
            extensions=extensions,
        )

    @staticmethod
    def _scan_raw_sections(text: str) -> dict[str, list[dict[str, Any]]]:
        raw_sections: dict[str, list[dict[str, Any]]] = {}
        current: str | None = None
        for line_number, line in enumerate(text.splitlines(), start=1):
            stripped = line.strip()
            if not stripped or stripped.startswith((";", "#")):
                continue
            match = _SECTION_RE.match(stripped)
            if match:
                current = match.group("name").strip()
                raw_sections.setdefault(current, [])
                continue
            if current is None or "=" not in line:
                continue
            key, value = line.split("=", 1)
            raw_sections[current].append(
                {"key": key.strip(), "value": value.strip(), "line": line_number}
            )
        return raw_sections

    def _build_widget(
        self,
        *,
        raw: bytes,
        source_format: str,
        section: str,
        items: Mapping[str, str],
        kind: str,
        metric: str,
        label: str,
        min_value: float | None,
        max_value: float | None,
        layout: Mapping[str, Any],
        style: Mapping[str, Any] | None,
        typography: Mapping[str, Any] | None,
        unknown_keys: list[str],
        asset_references: list[dict[str, str]],
        seen_ids: set[str],
        suffix: str,
        measure_names: list[str] | None = None,
        unit: str = "",
    ) -> dict[str, Any]:
        widget_id = self._unique_id(f"rainmeter-{suffix}-{_slug(section)}", seen_ids)
        binding: dict[str, Any] = {"metric": metric, "source": "rainmeter"}
        if unit:
            binding["unit"] = unit
        domain: dict[str, Any] = {}
        if min_value is not None and max_value is not None and min_value <= max_value:
            domain = {"min": min_value, "max": max_value}
        script_file = _get(items, "ScriptFile")
        extensions = {
            "rainmeter": {
                "section": section,
                "section_name": section,
                "config": dict(items),
                "unknown_keys": unknown_keys,
                "measure_names": list(measure_names or []),
                "asset_references": list(asset_references),
                "script_file": script_file or "",
                "executable": False,
            }
        }
        widget = make_widget(
            widget_id,
            kind,
            source_format,
            raw,
            metric=metric,
            label=label,
            domain=domain or None,
            layout=layout or None,
            style=style,
            typography=typography,
            extensions=extensions,
        )
        widget["binding"].update(
            {key: value for key, value in binding.items() if key != "metric"}
        )
        return widget

    @staticmethod
    def _unique_id(candidate: str, seen: set[str]) -> str:
        unique = candidate
        index = 1
        while unique in seen:
            index += 1
            unique = f"{candidate}-{index}"
        seen.add(unique)
        return unique


__all__ = ["RainmeterImporter"]
