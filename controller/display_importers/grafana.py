# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless Grafana dashboard/panel JSON importer.

The importer accepts a dashboard document, a single panel document, or an API
export wrapper such as ``{"dashboard": {...}, "meta": {...}}``.  Panels are
mapped to best-effort :class:`WidgetIR`; queries, datasources, transformations,
``fieldConfig``/overrides, thresholds, units, templating, and the original JSON
are retained verbatim in ``extensions``.  Queries are never executed.
"""

from __future__ import annotations

import json
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

#: Hard cap before JSON parsing.
MAX_SOURCE_BYTES = 8 * 1024 * 1024

FORMAT = "grafana_json"
KIND_DASHBOARD = "dashboard"
KIND_PANEL = "panel"

_PANEL_KINDS = {
    "gauge": "gauge",
    "stat": "number",
    "singlestat": "number",
    "bargauge": "bar",
    "bargraph": "bar",
    "timeseries": "chart",
    "graph": "chart",
    "table": "table",
    "text": "text",
}

_ROW_TYPES = frozenset({"row"})

_DASHBOARD_METADATA_KEYS = (
    "id",
    "uid",
    "title",
    "description",
    "tags",
    "timezone",
    "schemaVersion",
    "version",
    "templating",
    "annotations",
    "links",
    "time",
    "timepicker",
    "refresh",
    "editable",
    "graphTooltip",
    "panels",
    "rows",
)

_UNSAFE_CONSTANTS = {"NaN", "Infinity", "-Infinity"}


def _reject_constant(value: str) -> None:
    if value in _UNSAFE_CONSTANTS:
        raise ImportError(f"non-finite JSON constant `{value}` is not supported")
    raise ImportError(f"invalid JSON constant `{value}`")


def _load_json(data: bytes) -> Any:
    try:
        text = data.decode("utf-8-sig")
        return json.loads(text, parse_constant=_reject_constant)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ImportError(f"invalid Grafana JSON: {exc}") from exc


def _slug(value: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")
    return slug or "panel"


def _as_number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if number != number or number in {float("inf"), float("-inf")}:
        return None
    return number


def _first_string(*values: Any) -> str:
    for value in values:
        if isinstance(value, str) and value.strip():
            return value
    return ""


def _datasource_source(datasource: Any) -> str:
    if isinstance(datasource, str):
        return datasource
    if isinstance(datasource, Mapping):
        return _first_string(
            datasource.get("uid"), datasource.get("type"), datasource.get("name")
        )
    return ""


def _thresholds(panel: Mapping[str, Any]) -> Any:
    field_config = panel.get("fieldConfig")
    if isinstance(field_config, Mapping):
        defaults = field_config.get("defaults")
        if isinstance(defaults, Mapping) and defaults.get("thresholds") is not None:
            return defaults.get("thresholds")
    options = panel.get("options")
    if isinstance(options, Mapping) and options.get("thresholds") is not None:
        return options.get("thresholds")
    if panel.get("thresholds") is not None:
        return panel.get("thresholds")
    return None


def _threshold_domain(thresholds: Any) -> list[dict[str, Any]]:
    steps: Any = thresholds
    if isinstance(thresholds, Mapping):
        steps = thresholds.get("steps")
    if not isinstance(steps, list):
        return []
    result: list[dict[str, Any]] = []
    for index, step in enumerate(steps):
        if not isinstance(step, Mapping):
            continue
        at = _as_number(step.get("value"))
        if at is None:
            # Grafana's first base step has a null value.
            continue
        color = _first_string(step.get("color"), step.get("severity"))
        severity = color or f"step-{index}"
        entry: dict[str, Any] = {"at": at, "severity": severity}
        if color:
            entry["label"] = color
        result.append(entry)
    return result


def _domain(panel: Mapping[str, Any]) -> dict[str, Any] | None:
    domain: dict[str, Any] = {}
    field_config = panel.get("fieldConfig")
    if isinstance(field_config, Mapping):
        defaults = field_config.get("defaults")
        if isinstance(defaults, Mapping):
            for key in ("min", "max"):
                number = _as_number(defaults.get(key))
                if number is not None:
                    domain[key] = number
    options = panel.get("options")
    if isinstance(options, Mapping):
        for key in ("min", "max"):
            if key in domain:
                continue
            number = _as_number(options.get(key))
            if number is not None:
                domain[key] = number
    if (
        "min" in domain
        and "max" in domain
        and float(domain["min"]) > float(domain["max"])
    ):
        domain.pop("min")
        domain.pop("max")
    mapped_thresholds = _threshold_domain(_thresholds(panel))
    if mapped_thresholds:
        domain["thresholds"] = mapped_thresholds
    return domain or None


def _layout(panel: Mapping[str, Any]) -> dict[str, Any]:
    grid = panel.get("gridPos")
    if not isinstance(grid, Mapping):
        return {}
    layout: dict[str, Any] = {}
    for source_key, target_key in (
        ("x", "x"),
        ("y", "y"),
        ("w", "width"),
        ("h", "height"),
    ):
        number = _as_number(grid.get(source_key))
        if number is None:
            continue
        if target_key in {"width", "height"} and number <= 0:
            continue
        layout[target_key] = number
    return layout


def _overrides(panel: Mapping[str, Any]) -> Any:
    field_config = panel.get("fieldConfig")
    if isinstance(field_config, Mapping) and field_config.get("overrides") is not None:
        return field_config.get("overrides")
    if panel.get("overrides") is not None:
        return panel.get("overrides")
    return []


def _unit(panel: Mapping[str, Any]) -> str:
    field_config = panel.get("fieldConfig")
    if isinstance(field_config, Mapping):
        defaults = field_config.get("defaults")
        if isinstance(defaults, Mapping):
            unit = _first_string(defaults.get("unit"))
            if unit:
                return unit
    options = panel.get("options")
    if isinstance(options, Mapping):
        unit = _first_string(options.get("unit"))
        if unit:
            return unit
    return _first_string(panel.get("format"))


def _dashboard_metadata(
    dashboard: Mapping[str, Any], wrapper: Mapping[str, Any] | None
) -> dict[str, Any]:
    metadata: dict[str, Any] = {}
    for key in _DASHBOARD_METADATA_KEYS:
        if key in dashboard:
            metadata[key] = dashboard[key]
    if wrapper is not None:
        metadata["wrapper"] = {
            key: value for key, value in wrapper.items() if key != "dashboard"
        }
    return metadata


class GrafanaImporter(BaseImporter):
    """Import Grafana dashboards and panel JSON losslessly."""

    format = FORMAT
    version = ""
    suffixes = (".json",)
    mime = "application/json"

    def sniff(self, data: bytes) -> bool:
        lowered = data.lower()
        return any(
            token in lowered
            for token in (b'"dashboard"', b'"panels"', b'"targets"', b'"gridpos"')
        )

    def _asset(self, name: str, raw: bytes) -> ImportAsset:
        return ImportAsset(
            name or "grafana.json",
            raw,
            mime="application/json",
            kind="original",
            executable=False,
            sandbox="none",
        )

    def _classify(
        self, document: Mapping[str, Any]
    ) -> tuple[Mapping[str, Any], str, Mapping[str, Any] | None]:
        dashboard = document.get("dashboard")
        if isinstance(dashboard, Mapping):
            return dashboard, KIND_DASHBOARD, document
        panel = document.get("panel")
        if isinstance(panel, Mapping):
            return panel, KIND_PANEL, document
        if "type" in document:
            return document, KIND_PANEL, None
        if isinstance(document.get("panels"), list) or "templating" in document:
            return document, KIND_DASHBOARD, None
        return document, KIND_DASHBOARD, None

    def parse(
        self,
        name: str,
        data: bytes,
        context: ImportContext,
    ) -> ImportResult:
        raw = bytes(data)
        if len(raw) > MAX_SOURCE_BYTES:
            raise ImportError(
                f"Grafana source is {len(raw)} bytes; limit is {MAX_SOURCE_BYTES}"
            )
        document = _load_json(raw)
        if not isinstance(document, Mapping):
            raise ImportError("Grafana source must be a JSON object")
        dashboard, source_kind, wrapper = self._classify(document)
        asset = self._asset(name, data)
        warnings: list[str] = []
        widgets: list[dict[str, Any]] = []
        unknown_panels: list[dict[str, Any]] = []
        seen_ids: set[str] = set()
        counter = 0

        dashboard_meta = _dashboard_metadata(dashboard, wrapper)
        templating = dashboard.get("templating")
        version = str(
            dashboard.get("schemaVersion")
            or dashboard.get("version")
            or document.get("schemaVersion")
            or ""
        )

        def add_widget(panel: Mapping[str, Any], path: str) -> None:
            nonlocal counter
            panel_type = str(panel.get("type") or "").strip()
            kind = _PANEL_KINDS.get(panel_type.lower())
            if kind is None and panel_type.lower() in _ROW_TYPES:
                kind = "text"
            if kind == "number":
                options = panel.get("options")
                sparkline = (
                    options.get("sparkline") if isinstance(options, Mapping) else None
                )
                if isinstance(sparkline, Mapping) and sparkline.get("show") is True:
                    kind = "sparkline"
            if kind is None:
                unknown_panels.append(
                    {
                        "path": path,
                        "type": panel_type,
                        "panel": dict(panel),
                        "reason": "unsupported panel type",
                    }
                )
                warnings.append(
                    f"Grafana panel `{panel_type or 'untyped'}` at {path} "
                    "retained opaque without a widget"
                )
                return
            title = _first_string(panel.get("title"), panel.get("description"))
            label = title or panel_type or f"panel {counter}"
            base_id = f"grafana-{counter}-{_slug(title or panel_type or 'panel')}"
            widget_id = base_id
            suffix = 2
            while widget_id in seen_ids:
                widget_id = f"{base_id}-{suffix}"
                suffix += 1
            seen_ids.add(widget_id)
            counter += 1

            unit = _unit(panel)
            panel_thresholds = _thresholds(panel)
            panel_extensions = {
                "grafana": {
                    "panel": dict(panel),
                    "panel_id": panel.get("id"),
                    "panel_type": panel_type,
                    "path": path,
                    "targets": panel.get("targets"),
                    "queries": panel.get("targets"),
                    "datasource": panel.get("datasource"),
                    "transformations": panel.get("transformations"),
                    "fieldConfig": panel.get("fieldConfig"),
                    "overrides": _overrides(panel),
                    "thresholds": panel_thresholds,
                    "unit": unit,
                    "templating": templating,
                    "dashboard": dashboard_meta,
                    "queries_executed": False,
                }
            }
            widget = make_widget(
                widget_id,
                kind,
                FORMAT,
                data,
                metric=title or panel_type or widget_id,
                role=panel_type or "panel",
                label=label,
                domain=_domain(panel),
                layout=_layout(panel) or None,
                extensions=panel_extensions,
                source_version=version,
                source_unknown=dict(panel),
            )
            if unit:
                widget["binding"]["unit"] = unit
            datasource_source = _datasource_source(panel.get("datasource"))
            if datasource_source:
                widget["binding"]["source"] = datasource_source
            widgets.append(widget)

        def walk(panels: Any, path_prefix: str) -> None:
            if not isinstance(panels, list):
                return
            for index, panel in enumerate(panels):
                path = f"{path_prefix}[{index}]"
                if not isinstance(panel, Mapping):
                    unknown_panels.append(
                        {
                            "path": path,
                            "type": None,
                            "panel": panel,
                            "reason": "not an object",
                        }
                    )
                    warnings.append(f"Grafana panel at {path} is not an object")
                    continue
                nested = panel.get("panels")
                add_widget(panel, path)
                if isinstance(nested, list):
                    walk(nested, f"{path}.panels")

        if source_kind == KIND_DASHBOARD:
            walk(dashboard.get("panels"), "dashboard.panels")
            rows = dashboard.get("rows")
            if isinstance(rows, list):
                for index, row in enumerate(rows):
                    path = f"dashboard.rows[{index}]"
                    if isinstance(row, Mapping):
                        add_widget(row, path)
                        walk(row.get("panels"), f"{path}.panels")
                    else:
                        unknown_panels.append(
                            {
                                "path": path,
                                "type": None,
                                "panel": row,
                                "reason": "not an object",
                            }
                        )
        else:
            add_widget(dashboard, "panel")
            if isinstance(dashboard.get("panels"), list):
                walk(dashboard.get("panels"), "panel.panels")

        unknown = {
            "grafana": {
                "unknown_panels": unknown_panels,
                "queries_executed": False,
            }
        }
        extensions = {
            "grafana": {
                "kind": source_kind,
                "document": dict(document),
                "dashboard": dashboard_meta,
                "templating": templating,
                "unknown_panels": unknown_panels,
                "queries_executed": False,
                "context": {"name": context.name, "base_dir": context.base_dir},
            }
        }
        source_document = make_source_document(
            FORMAT,
            version,
            data,
            original=asset,
            parsed=dict(document),
            unknown=unknown,
        )
        source_capture = make_source_capture(
            FORMAT,
            version,
            data,
            assets=[asset],
            captured=True,
            opaque=unknown,
            warnings=warnings,
        )
        return ImportResult(
            format=FORMAT,
            version=version,
            raw=data,
            widgets=widgets,
            source_document=source_document,
            source_capture=source_capture,
            assets=[asset],
            unknown=unknown,
            warnings=warnings,
            extensions=extensions,
        )


#: Convenience alias matching the ``grafana_json`` source format name.
GrafanaJSONImporter = GrafanaImporter
