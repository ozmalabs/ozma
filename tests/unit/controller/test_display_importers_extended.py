# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for the MagicMirror, Grafana, and Adaptive Cards importers."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import display_ir as ir
from display_importers.adaptive_cards import (
    MAX_EXPORT_WIDGETS,
    AdaptiveCardError,
    AdaptiveCardsImporter,
    export_adaptive_card,
)
from display_importers.base import ImportError, make_widget, sha256_hex
from display_importers.grafana import GrafanaImporter
from display_importers.magicmirror import (
    MAX_SOURCE_BYTES,
    ConfigParseError,
    MagicMirrorImporter,
    parse_config_js,
)

pytestmark = pytest.mark.unit

_FIXTURES = Path(__file__).resolve().parents[2] / "fixtures" / "display_imports"
_MM_SENTINEL = Path("/tmp/ozma_magicmirror_executed")
_MM_INVALID_SENTINEL = Path("/tmp/ozma_magicmirror_invalid_executed")


def _fixture_bytes(name: str) -> bytes:
    return (_FIXTURES / name).read_bytes()


def _fixture_json(name: str) -> dict:
    return json.loads((_FIXTURES / name).read_text(encoding="utf-8"))


def _assert_lossless(result, raw: bytes) -> None:
    assert result.raw == raw
    assert result.lossless is True
    assert result.source_capture["lossless"] is True
    assert result.source_capture["captured"]
    assert result.source_document["source_hash"] == sha256_hex(raw)
    assert result.source_capture["source_hash"] == sha256_hex(raw)
    result.validate()
    for widget in result.widgets:
        ir.validate_ir(widget)
        assert widget["source"]["source_hash"] == sha256_hex(raw)


def _stable(result) -> str:
    return json.dumps(result.to_dict(), sort_keys=True, ensure_ascii=False)


def test_magicmirror_valid_config_maps_known_modules():
    _MM_SENTINEL.unlink(missing_ok=True)
    raw = _fixture_bytes("magicmirror_config.js")
    result = MagicMirrorImporter().import_path(_FIXTURES / "magicmirror_config.js")

    _assert_lossless(result, raw)
    assert not _MM_SENTINEL.exists(), "config.js must never be executed"
    assert result.format == "magicmirror_js"
    assert len(result.widgets) == 9

    by_role = {widget["role"]: widget for widget in result.widgets}
    assert by_role["clock"]["kind"] == "text"
    assert by_role["currentweather"]["kind"] == "icon"
    assert by_role["weatherforecast"]["kind"] == "table"
    assert by_role["calendar"]["kind"] == "table"
    assert by_role["newsfeed"]["kind"] == "table"
    assert by_role["compliments"]["kind"] == "text"
    assert by_role["alert"]["kind"] == "card"
    assert by_role["updatenotification"]["kind"] == "text"
    assert by_role["MMM-Remote-Control"]["kind"] == "card"

    clock = by_role["clock"]["extensions"]["magicmirror"]
    assert clock["module"] == "clock"
    assert clock["config"]["config"]["displaySeconds"] is True
    assert by_role["clock"]["source"]["unknown"]["module"] == "clock"

    unknown_modules = result.extensions["magicmirror"]["unknown_modules"]
    assert [item["module"] for item in unknown_modules] == ["totally-unknown"]
    assert unknown_modules[0]["raw"]["config"] == {"foo": "bar"}

    references = result.extensions["magicmirror"]["references"]
    assert any(
        ref["value"] == "css/custom.css" and ref["kind"] == "css" for ref in references
    )
    assert any(ref["value"] == "assets/logo.svg" for ref in references)
    assert all(ref["inline"] is False for ref in references)
    assert any(
        ref["remote"] is True
        and ref["value"] == "https://api.example.com/weather"
        for ref in references
    )
    assert result.extensions["magicmirror"]["executed"] is False

    # Raw modules and raw source remain available for lossless re-export.
    assert len(result.extensions["magicmirror"]["modules"]) == 10
    assert result.source_document["parsed"]["port"] == 8080
    assert result.assets[0].kind == "script"
    assert result.assets[0].executable is True
    assert result.assets[0].sandbox == "iframe"


def test_magicmirror_invalid_config_is_opaque_and_not_executed():
    _MM_INVALID_SENTINEL.unlink(missing_ok=True)
    raw = _fixture_bytes("magicmirror_invalid.js")
    result = MagicMirrorImporter().import_path(_FIXTURES / "magicmirror_invalid.js")

    _assert_lossless(result, raw)
    assert not _MM_INVALID_SENTINEL.exists()
    assert result.widgets == []
    assert result.source_document["parsed"] is None
    assert "could not be parsed" in result.warnings[0]
    opaque = result.unknown["magicmirror"]["opaque_source"]
    assert "var config" in opaque
    assert result.extensions["magicmirror"]["executed"] is False
    assert "parse_error" in result.extensions["magicmirror"]


def test_magicmirror_bounded_parser_limits():
    depth = 70
    deep = "var config = " + "{a:" * depth + "1" + "}" * depth
    with pytest.raises(ConfigParseError, match="depth limit"):
        parse_config_js(deep)

    oversized = (
        "var config = { modules: [] };" + " " * (MAX_SOURCE_BYTES + 1)
    ).encode()
    result = MagicMirrorImporter().import_source("config.js", oversized)
    assert result.lossless is True
    assert result.widgets == []
    assert "size limit" in result.warnings[0]

    with pytest.raises(ConfigParseError, match="no `config"):
        parse_config_js("var other = { modules: [] };")


def test_magicmirror_output_is_deterministic():
    first = MagicMirrorImporter().import_path(_FIXTURES / "magicmirror_config.js")
    second = MagicMirrorImporter().import_path(_FIXTURES / "magicmirror_config.js")
    assert _stable(first) == _stable(second)
    assert [w["id"] for w in first.widgets] == [w["id"] for w in second.widgets]


def test_grafana_dashboard_nested_panels_and_retention():
    raw = _fixture_bytes("grafana_dashboard.json")
    result = GrafanaImporter().import_path(_FIXTURES / "grafana_dashboard.json")

    _assert_lossless(result, raw)
    assert result.format == "grafana_json"
    assert result.version == "39"
    assert result.extensions["grafana"]["kind"] == "dashboard"
    assert len(result.widgets) == 8

    by_label = {widget["label"]: widget for widget in result.widgets}
    expected = {
        "Overview": "text",
        "CPU": "gauge",
        "Uptime": "number",
        "Queue Depth": "bar",
        "Traffic": "chart",
        "Legacy Graph": "chart",
        "Table": "table",
        "Notes": "text",
    }
    for label, kind in expected.items():
        assert by_label[label]["kind"] == kind

    cpu = by_label["CPU"]
    grafana = cpu["extensions"]["grafana"]
    assert grafana["targets"][0]["expr"].startswith("100 - avg(rate(")
    assert grafana["targets"][0]["legendFormat"] == "CPU"
    assert grafana["datasource"] == {"type": "prometheus", "uid": "prom-1"}
    assert grafana["transformations"] == [
        {"id": "organize", "options": {"excludeByName": {"Time": True}}}
    ]
    assert grafana["fieldConfig"]["defaults"]["unit"] == "percent"
    assert grafana["overrides"][0]["matcher"]["id"] == "byName"
    assert grafana["thresholds"]["mode"] == "absolute"
    assert grafana["queries_executed"] is False
    assert cpu["binding"]["unit"] == "percent"
    assert cpu["domain"]["min"] == 0
    assert cpu["domain"]["max"] == 100
    thresholds = {
        item["at"]: item["severity"] for item in cpu["domain"]["thresholds"]
    }
    assert thresholds == {70.0: "yellow", 90.0: "red"}
    assert cpu["layout"] == {"x": 0, "y": 1, "width": 8, "height": 8}

    dashboard = result.extensions["grafana"]["dashboard"]
    assert dashboard["uid"] == "fixture-uid"
    assert dashboard["templating"]["list"][0]["query"].startswith("label_values")
    assert result.extensions["grafana"]["templating"]["list"][0]["name"] == "instance"
    assert result.extensions["grafana"]["document"]["meta"]["slug"] == (
        "fixture-dashboard"
    )
    assert result.extensions["grafana"]["queries_executed"] is False

    unknown = result.unknown["grafana"]["unknown_panels"]
    assert [item["type"] for item in unknown] == ["heatmap"]
    assert unknown[0]["panel"]["title"] == "Unsupported Heatmap"


def test_grafana_standalone_panel_and_raw_dashboard():
    raw = _fixture_bytes("grafana_panel.json")
    panel_result = GrafanaImporter().import_path(_FIXTURES / "grafana_panel.json")
    _assert_lossless(panel_result, raw)
    assert panel_result.format == "grafana_json"
    assert panel_result.version == "39"
    assert panel_result.extensions["grafana"]["kind"] == "panel"
    assert len(panel_result.widgets) == 1
    widget = panel_result.widgets[0]
    assert widget["kind"] == "gauge"
    assert widget["label"] == "Standalone Pressure"
    thresholds = {item["at"] for item in widget["domain"]["thresholds"]}
    assert thresholds == {100.0}
    assert widget["extensions"]["grafana"]["targets"][0]["expr"] == "pressure_pa"

    document = _fixture_json("grafana_dashboard.json")["dashboard"]
    dashboard_raw = json.dumps(document).encode()
    dashboard_result = GrafanaImporter().import_source(
        "dashboard.json", dashboard_raw
    )
    _assert_lossless(dashboard_result, dashboard_raw)
    assert dashboard_result.format == "grafana_json"
    assert len(dashboard_result.widgets) == 8

    with pytest.raises(ImportError, match="invalid Grafana JSON"):
        GrafanaImporter().import_source("broken.json", b"{not-json")


def test_grafana_output_is_deterministic():
    first = GrafanaImporter().import_path(_FIXTURES / "grafana_dashboard.json")
    second = GrafanaImporter().import_path(_FIXTURES / "grafana_dashboard.json")
    assert _stable(first) == _stable(second)
    assert [w["id"] for w in first.widgets] == [w["id"] for w in second.widgets]


def test_adaptive_card_import_supported_and_unsupported():
    raw = _fixture_bytes("adaptive_card.json")
    result = AdaptiveCardsImporter().import_path(_FIXTURES / "adaptive_card.json")

    _assert_lossless(result, raw)
    assert result.format == "adaptive_card"
    assert result.version == "1.5"
    assert len(result.widgets) == 13

    roles = [widget["role"] for widget in result.widgets]
    assert roles[:4] == ["TextBlock", "FactSet", "Image", "Media"]
    assert roles.count("Container") == 1
    assert roles.count("ColumnSet") == 1
    assert roles.count("Column") == 2
    assert roles.count("ActionSet") == 2
    assert roles.count("TextBlock") == 4
    assert "Input.Text" not in roles
    assert "RichTextBlock" not in roles

    unsupported = result.extensions["adaptive_card"]["unsupported"]
    unsupported_types = {item["type"] for item in unsupported}
    assert {"Input.Text", "RichTextBlock"} <= unsupported_types
    unsupported_actions = result.extensions["adaptive_card"]["unsupported_actions"]
    assert [item["type"] for item in unsupported_actions] == ["Action.Submit"]

    action_sets = [
        widget for widget in result.widgets if widget["role"] == "ActionSet"
    ]
    body_action_set = action_sets[0]["extensions"]["adaptive_card"]["element"]
    assert body_action_set["actions"][1]["type"] == "Action.Submit"
    assert body_action_set["actions"][1]["data"] == {"script": "alert(1)"}
    assert action_sets[1]["extensions"]["adaptive_card"]["path"] == ["actions", 0]

    document = result.extensions["adaptive_card"]["document"]
    assert document["metadata"] == {"fixture": True}
    assert document["body"][0]["text"] == "Hello **Ozma**"
    assert result.extensions["adaptive_card"]["executed"] is False


def test_adaptive_card_export_round_trip_and_report():
    imported = AdaptiveCardsImporter().import_path(_FIXTURES / "adaptive_card.json")
    exported = export_adaptive_card(imported.widgets)
    card = exported.card
    report = exported.report

    ir.validate_translation_report(report)
    assert report["direction"] == "export"
    assert report["target_format"] == "adaptive_card"
    assert report["fidelity"] in {"semantic", "approximate"}
    assert report["converted"]
    assert any("Action.Submit" in item for item in report["dropped"])
    assert exported["translation_report"] is report
    assert exported.to_dict()["card"] is card
    unpacked_card, unpacked_report = exported
    assert unpacked_card is card
    assert unpacked_report is report

    assert card["type"] == "AdaptiveCard"
    assert card["version"] == "1.5"
    assert card["$schema"] == "http://adaptivecards.io/schemas/adaptive-card.json"
    body_types = [item["type"] for item in card["body"]]
    for element_type in (
        "TextBlock",
        "FactSet",
        "Image",
        "Media",
        "Container",
        "ColumnSet",
        "Column",
        "ActionSet",
    ):
        assert element_type in body_types

    serialized = json.dumps(card)
    assert "Input.Text" not in serialized
    assert "RichTextBlock" not in serialized
    assert "Action.Submit" not in serialized
    assert "alert(1)" not in serialized
    assert '"script"' not in serialized

    reimported = AdaptiveCardsImporter().import_source(
        "exported.json", serialized.encode()
    )
    _assert_lossless(reimported, serialized.encode())
    assert reimported.widgets
    assert "Input.Text" not in [w["role"] for w in reimported.widgets]


def test_adaptive_card_export_accepts_layout_and_is_deterministic():
    imported = AdaptiveCardsImporter().import_path(_FIXTURES / "adaptive_card.json")
    layout = ir.to_screen_layout(imported.widgets[0])
    first = export_adaptive_card(layout)
    second = export_adaptive_card(layout)
    assert first.card == second.card
    assert first.report == second.report
    assert first.card["body"][0]["type"] == "TextBlock"


def test_adaptive_card_export_approximates_gauges_and_charts():
    widget = make_widget(
        "gauge-1",
        "gauge",
        "test.fixture",
        b"{}",
        label="Pressure",
    )
    exported = export_adaptive_card([widget])
    report = exported.report
    ir.validate_translation_report(report)
    assert report["fidelity"] == "approximate"
    assert any("gauge" in item for item in report["approximated"])
    assert any("gauge" in item for item in report["dropped"])
    assert exported.card["body"] == [
        {"type": "TextBlock", "text": "Pressure", "wrap": True}
    ]


def test_adaptive_card_export_strips_executable_fields():
    widget = make_widget(
        "text-1",
        "text",
        "test.fixture",
        b"{}",
        label="Safe",
        extensions={
            "adaptive_card": {
                "element": {
                    "type": "TextBlock",
                    "text": "Safe",
                    "script": "alert(1)",
                    "onClick": "alert(2)",
                    "selectAction": {
                        "type": "Action.Submit",
                        "title": "Run",
                        "data": {"script": "alert(3)"},
                    },
                }
            }
        },
    )
    exported = export_adaptive_card([widget])
    element = exported.card["body"][0]
    assert element == {"type": "TextBlock", "text": "Safe"}
    assert set(element) == {"type", "text"}
    assert exported.report["fidelity"] == "approximate"
    assert any("executable" in item for item in exported.report["dropped"])


def test_adaptive_card_export_rejects_unbounded_sizes():
    widget = make_widget("text-1", "text", "test.fixture", b"{}", label="Safe")
    with pytest.raises(AdaptiveCardError):
        export_adaptive_card([widget] * (MAX_EXPORT_WIDGETS + 1))

    large = make_widget(
        "text-large",
        "text",
        "test.fixture",
        b"{}",
        label="x" * 5000,
    )
    with pytest.raises(AdaptiveCardError, match="limit"):
        export_adaptive_card([large], {"max_bytes": 100})

    with pytest.raises(AdaptiveCardError, match="1.5"):
        export_adaptive_card([widget], {"version": "1.6"})
