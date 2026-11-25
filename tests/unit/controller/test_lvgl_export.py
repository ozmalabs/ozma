# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for the deterministic LVGL C/JSON exporter."""

from __future__ import annotations

import math

import pytest

from lvgl_export import (
    LVGL_EXPORT_SCHEMA,
    LVGL_TARGET_FORMAT,
    LvglExportError,
    MAX_WIDGETS,
    export_lvgl,
    export_lvgl_json,
)

pytestmark = pytest.mark.unit


def _layout(widgets, **overrides):
    layout = {
        "id": "test-layout",
        "width": 320,
        "height": 240,
        "background": "#0a0a0f",
        "widgets": widgets,
    }
    layout.update(overrides)
    return layout


def _gauge(**overrides):
    widget = {
        "type": "gauge",
        "x": 10,
        "y": 10,
        "w": 180,
        "h": 180,
        "metric": "cpu",
        "min": 0,
        "max": 100,
        "label": "CPU",
        "unit": "%",
        "decimals": 0,
        "warn": 70,
        "crit": 90,
    }
    widget.update(overrides)
    return widget


def _bar(**overrides):
    widget = {
        "type": "bar",
        "x": 10,
        "y": 210,
        "w": 220,
        "h": 24,
        "metric": "mem",
        "min": 0,
        "max": 100,
        "label": "Memory",
        "unit": "%",
        "decimals": 0,
        "warn": 80,
    }
    widget.update(overrides)
    return widget


def _text(**overrides):
    widget = {
        "type": "label",
        "x": 0,
        "y": 0,
        "w": 200,
        "h": 30,
        "text": "Node {node_name}",
        "font_size": 16,
    }
    widget.update(overrides)
    return widget


def _chart(**overrides):
    widget = {
        "type": "sparkline",
        "x": 0,
        "y": 40,
        "w": 200,
        "h": 80,
        "metric": "history",
        "min": 0,
        "max": 100,
        "values": [1, 20, 50, 100],
        "decimals": 0,
    }
    widget.update(overrides)
    return widget


# ---------------------------------------------------------------------------
# Core widget export
# ---------------------------------------------------------------------------


def test_gauge_exports_arc_thresholds_and_setter():
    result = export_lvgl(_layout([_gauge()]))
    source = result["source"]
    header = result["header"]
    metadata = result["metadata"]

    assert "lv_arc_create" in source
    assert "lv_arc_set_bg_angles" in source
    assert "lv_arc_set_value" in source
    assert "ozma_lvgl_threshold_t" in source
    assert "ozma_lvgl_threshold_t" in header
    assert "ozma_lvgl_init" in header
    assert "set_value" in header
    assert "0x5b6fff" in source
    assert "70.0f" in source
    assert "90.0f" in source
    assert metadata["counts"]["widgets"] == 1
    assert metadata["widgets"][0]["kind"] == "gauge"
    assert metadata["widgets"][0]["target"] == "lv_arc"
    assert [item["at"] for item in metadata["widgets"][0]["thresholds"]] == [70.0, 90.0]


def test_bar_and_number_export_expected_lvgl_calls():
    result = export_lvgl(_layout([
        _bar(),
        {
            "type": "number",
            "x": 0,
            "y": 100,
            "w": 120,
            "h": 40,
            "metric": "temp",
            "min": 0,
            "max": 120,
            "value": 42.5,
            "unit": "celsius",
            "decimals": 1,
        },
    ]))
    source = result["source"]
    assert "lv_bar_create" in source
    assert "lv_bar_set_range" in source
    assert "lv_bar_set_value" in source
    assert "lv_label_create" in source
    assert "42.5" in source
    assert "set_value(float value)" in source
    assert "lv_label_set_text_fmt" in source
    assert result["metadata"]["counts"]["widgets"] == 2


def test_text_and_chart_export_static_arrays():
    result = export_lvgl(_layout([_text(), _chart()]))
    source = result["source"]
    metadata = result["metadata"]

    assert "lv_label_create" in source
    assert "Node {node_name}" in source
    assert "lv_chart_create" in source
    assert "lv_chart_add_series" in source
    assert "static const lv_coord_t" in source
    assert "POINT_COUNT 4" in source
    assert metadata["counts"]["points"] == 4
    assert metadata["widgets"][1]["target"] == "lv_chart"


def test_export_lvgl_json_returns_metadata_only():
    metadata = export_lvgl_json(_layout([_gauge()]))
    assert metadata["schema"] == LVGL_EXPORT_SCHEMA
    assert metadata["target_format"] == LVGL_TARGET_FORMAT
    assert "source" not in metadata
    assert "header" not in metadata
    assert metadata["widgets"][0]["identifier"].startswith("ozma_lvgl_")


# ---------------------------------------------------------------------------
# Escaping and bounds
# ---------------------------------------------------------------------------


def test_identifiers_and_strings_are_escaped():
    malicious = {
        "type": "text",
        "ir_id": 'bad-id";}\n/*',
        "x": 0,
        "y": 0,
        "w": 100,
        "h": 30,
        "text": 'quote " newline\n tab\t snowman: \u2603 \\ end',
    }
    result = export_lvgl(_layout([malicious]))
    source = result["source"]
    identifier = result["metadata"]["widgets"][0]["identifier"]

    assert '"' not in identifier
    assert "\n" not in identifier
    assert identifier == "ozma_lvgl_bad_id_0"
    assert 'quote \\" newline\\n tab\\t snowman:' in source
    assert "\\342\\230\\203" in source  # UTF-8 octal escapes for the snowman
    assert 'bad-id";}' not in source


def test_large_strings_and_non_finite_values_are_rejected():
    with pytest.raises(LvglExportError):
        export_lvgl(_layout([{
            "type": "text",
            "x": 0,
            "y": 0,
            "w": 100,
            "h": 30,
            "text": "x" * 5000,
        }]))
    with pytest.raises(LvglExportError):
        export_lvgl(_layout([_gauge(x=math.inf)]))
    with pytest.raises(LvglExportError):
        export_lvgl(_layout([_gauge(w=9000)]))


def test_widget_count_is_bounded():
    widgets = [{"type": "text", "w": 1, "h": 1} for _ in range(MAX_WIDGETS + 1)]
    with pytest.raises(LvglExportError):
        export_lvgl({"widgets": widgets})


# ---------------------------------------------------------------------------
# Threshold colours and report shape
# ---------------------------------------------------------------------------


def test_threshold_colours_are_exported_as_runtime_setters():
    widget = _gauge(
        warn=None,
        crit=None,
        domain={
            "min": 0,
            "max": 100,
            "thresholds": [
                {"at": 80, "severity": "warning", "color": "#ffaa00"},
                {"at": 95, "severity": "critical", "color": "#ff0000"},
            ],
        },
    )
    result = export_lvgl(_layout([widget]))
    source = result["source"]
    assert ">= 95.0f" in source
    assert ">= 80.0f" in source
    assert "0xff0000" in source
    assert "0xffaa00" in source
    assert [item["color"] for item in result["metadata"]["widgets"][0]["thresholds"]] == [
        "#ffaa00",
        "#ff0000",
    ]


def test_report_has_export_direction_hashes_and_approximations():
    result = export_lvgl(_layout([_gauge(), _bar(), _text(), _chart()]))
    metadata = result["metadata"]
    report = metadata["translation_report"]

    assert report["direction"] == "export"
    assert report["source_format"]
    assert report["target_format"] == LVGL_TARGET_FORMAT
    assert report["translator"]
    assert report["translator_version"]
    assert report["fidelity"] in {"semantic", "approximate"}
    assert report["converted"]
    assert any("gauge" in item for item in report["converted"])
    assert report["approximated"]
    assert isinstance(report["dropped"], list)
    assert isinstance(report["warnings"], list)
    assert len(metadata["source_hash"]) == 64
    assert len(metadata["ir_hash"]) == 64
    assert metadata["source_hash"] != metadata["ir_hash"]


def test_widget_ir_input_uses_layout_hint_and_domain():
    widget_ir = {
        "id": "org.ozma.widget.cpu_temp",
        "kind": "gauge",
        "binding": {"metric": "org.ozma/node.cpu_temp", "unit": "celsius"},
        "domain": {
            "min": 0,
            "max": 120,
            "thresholds": [{"at": 80, "severity": "warning", "color": "#ffaa00"}],
        },
        "label": "CPU",
        "layout": {"x": 12, "y": 14, "width": 120, "height": 120},
        "source": {"format": "ozma_display_definition", "source_hash": "0" * 64},
    }
    result = export_lvgl(widget_ir)
    metadata = result["metadata"]
    assert metadata["widgets"][0]["x"] == 12
    assert metadata["widgets"][0]["y"] == 14
    assert metadata["widgets"][0]["w"] == 120
    assert metadata["widgets"][0]["max"] == 120
    assert metadata["widgets"][0]["thresholds"][0]["color"] == "#ffaa00"
    assert "120" in result["source"]


def test_definition_wrapper_and_theme_tokens_are_accepted():
    definition = {
        "id": "scene",
        "version": 1,
        "layout": _layout([_gauge(color="{color.accent}")]),
        "source_format": "ozma_display_definition",
    }
    result = export_lvgl(
        definition,
        options={
            "theme": {
                "tokens": {
                    "color": {
                        "accent": {"$type": "color", "$value": "#123456"},
                    }
                }
            }
        },
    )
    assert "0x123456" in result["source"]
    assert result["metadata"]["translation_report"]["source_format"] == "ozma_display_definition"


# ---------------------------------------------------------------------------
# Determinism and malformed input
# ---------------------------------------------------------------------------


def test_export_is_deterministic():
    layout = _layout([_gauge(), _bar(), _text(), _chart()])
    first = export_lvgl(layout)
    second = export_lvgl(layout)
    assert first == second
    assert first["source"] == second["source"]
    assert first["header"] == second["header"]


def test_malformed_inputs_are_rejected():
    with pytest.raises(LvglExportError):
        export_lvgl(None)
    with pytest.raises(LvglExportError):
        export_lvgl({})
    with pytest.raises(LvglExportError):
        export_lvgl({"widgets": []})
    with pytest.raises(LvglExportError):
        export_lvgl({"widgets": ["not-an-object"]})
    with pytest.raises(LvglExportError):
        export_lvgl(_layout([_gauge(x="left")]))
    with pytest.raises(LvglExportError):
        export_lvgl(_layout([{"type": "table", "rows": [{"bad": "cell"}]}]))
    with pytest.raises(LvglExportError):
        export_lvgl(_layout([], width=math.nan))


def test_table_and_unknown_widgets_fall_back_to_labels():
    result = export_lvgl(_layout([
        {
            "type": "table",
            "x": 0,
            "y": 0,
            "w": 200,
            "h": 100,
            "label": "Facts",
            "rows": [["cpu", "42"], ["mem", "50"]],
        },
        {"type": "mystery", "x": 0, "y": 110, "w": 100, "h": 40, "label": "Unknown"},
    ]))
    source = result["source"]
    assert "lv_obj_create" in source
    assert "static const char *const" in source
    assert "ROW_COUNT 2" in source
    assert result["metadata"]["translation_report"]["fidelity"] == "approximate"
    assert any(
        "flattened" in item
        for item in result["metadata"]["translation_report"]["approximated"]
    )
