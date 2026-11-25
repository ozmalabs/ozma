# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for the controller-side display Widget IR contracts."""

from __future__ import annotations

import copy
import json
from pathlib import Path

import pytest

import display_ir as ir

pytestmark = pytest.mark.unit

_FIXTURES = Path(__file__).resolve().parents[2] / "fixtures" / "display_ir"
_SCHEMAS = Path(__file__).resolve().parents[3] / "schemas" / "display"


def _fixture(name: str) -> dict:
    return json.loads((_FIXTURES / name).read_text(encoding="utf-8"))


def _schema(name: str) -> dict:
    return json.loads((_SCHEMAS / name).read_text(encoding="utf-8"))


def _widget_ir() -> dict:
    return _fixture("widget_ir.json")


def _source_capture() -> dict:
    return _fixture("source_capture.json")


def _translation_report() -> dict:
    return _fixture("translation_report.json")


def _asset_pack() -> dict:
    return _fixture("asset_pack.json")


def test_golden_fixtures_validate_in_python():
    widget = _widget_ir()
    ir.validate_ir(widget)
    ir.validate_source(widget["source"])
    ir.validate_vibe(widget["vibe"])
    ir.validate_shape(widget["shape"])
    ir.validate_source_capture(_source_capture())
    ir.validate_translation_report(_translation_report())
    ir.validate_asset_pack(_asset_pack())
    ir.validate_sync_policy({"mode": "offline"})
    assert ir.canonical_hash(widget["source"]["parsed"]).__len__() == 64


@pytest.mark.parametrize(
    ("fixture", "schema"),
    [
        ("widget_ir.json", "ir-v1.schema.json"),
        ("source_capture.json", "source-v1.schema.json"),
        ("translation_report.json", "report-v1.schema.json"),
        ("asset_pack.json", "asset-pack-v1.schema.json"),
        ("vibe.json", "vibe-v1.schema.json"),
        ("font_asset.json", "font-v1.schema.json"),
    ],
)
def test_json_schemas_accept_golden_fixtures(fixture: str, schema: str):
    jsonschema = pytest.importorskip("jsonschema")
    jsonschema.Draft202012Validator.check_schema(_schema(schema))
    jsonschema.validate(
        instance=_fixture(fixture),
        schema=_schema(schema),
        cls=jsonschema.Draft202012Validator,
    )


def test_json_schemas_reject_lossy_imports_and_import_reports():
    jsonschema = pytest.importorskip("jsonschema")
    validator = jsonschema.Draft202012Validator(_schema("source-v1.schema.json"))
    lossy = _source_capture()
    lossy["lossless"] = False
    with pytest.raises(jsonschema.ValidationError):
        validator.validate(lossy)

    report_validator = jsonschema.Draft202012Validator(_schema("report-v1.schema.json"))
    import_report = _translation_report()
    import_report["direction"] = "import"
    with pytest.raises(jsonschema.ValidationError):
        report_validator.validate(import_report)
    missing_direction = _translation_report()
    missing_direction.pop("direction")
    with pytest.raises(jsonschema.ValidationError):
        report_validator.validate(missing_direction)

    ir_validator = jsonschema.Draft202012Validator(_schema("ir-v1.schema.json"))
    missing_source = _widget_ir()
    missing_source.pop("source")
    with pytest.raises(jsonschema.ValidationError):
        ir_validator.validate(missing_source)


def test_ir_rejects_missing_source_unknown_kinds_and_bad_assets():
    missing_source = _widget_ir()
    del missing_source["source"]
    with pytest.raises(ir.DisplayIrError):
        ir.validate_ir(missing_source)

    unknown_kind = _widget_ir()
    unknown_kind["kind"] = "hologram"
    with pytest.raises(ir.DisplayIrError, match="unknown widget kind"):
        ir.validate_ir(unknown_kind)

    bad_sha = _widget_ir()
    bad_sha["assets"][0]["sha256"] = "not-a-digest"
    with pytest.raises(ir.DisplayIrError, match="SHA-256"):
        ir.validate_ir(bad_sha)

    duplicate = _widget_ir()
    duplicate["assets"].append(copy.deepcopy(duplicate["assets"][0]))
    with pytest.raises(ir.DisplayIrError, match="duplicate asset id"):
        ir.validate_ir(duplicate)

    executable = _widget_ir()
    executable["assets"][0]["executable"] = True
    executable["assets"][0]["sandbox"] = "none"
    with pytest.raises(ir.DisplayIrError, match="sandbox"):
        ir.validate_ir(executable)


def test_imports_must_be_lossless_and_exports_must_have_reports():
    capture = _source_capture()
    capture["lossless"] = False
    with pytest.raises(ir.DisplayIrError, match="lossless"):
        ir.validate_source_capture(capture)

    report = _translation_report()
    report["direction"] = "import"
    with pytest.raises(ir.DisplayIrError, match="direction"):
        ir.validate_translation_report(report)

    missing_direction = _translation_report()
    missing_direction.pop("direction")
    with pytest.raises(ir.DisplayIrError, match="direction"):
        ir.validate_translation_report(missing_direction)

    with pytest.raises(ir.DisplayIrError, match="requires a TranslationReport"):
        ir.require_export_report(None, "browser_svg")

    with pytest.raises(ir.DisplayIrError, match="requires a TranslationReport"):
        ir.validate_asset_pack_for_export(_asset_pack(), "lvgl")

    lossy_pack = _asset_pack()
    lossy_pack["source_capture"]["lossless"] = False
    with pytest.raises(ir.DisplayIrError, match="lossless"):
        ir.validate_asset_pack(lossy_pack)

    no_reports = _asset_pack()
    no_reports["translation_reports"] = []
    with pytest.raises(ir.DisplayIrError, match="translation_reports"):
        ir.validate_asset_pack(no_reports)


def test_vibe_unknown_keywords_are_retained():
    profile = _fixture("vibe.json")
    ir.validate_vibe(profile)
    assert "not_in_taxonomy" in profile["keywords"]
    assert profile["custom_axis"] == ["retro"]
    encoded = json.dumps(profile)
    assert "not_in_taxonomy" in encoded
    assert "custom_axis" in encoded


def test_shape_and_coherence_bounds_are_enforced():
    coherence = {
        "score": 0.93,
        "dimensions": {"mood": 0.9, "color": 0.95},
        "confidence": 0.8,
    }
    ir.validate_coherence(coherence)

    for invalid in (
        {"score": 1.2, "confidence": 0.8},
        {"score": 0.9, "confidence": -0.1},
        {"score": 0.9, "dimensions": {"mood": 1.5}, "confidence": 0.8},
    ):
        with pytest.raises(ir.DisplayIrError):
            ir.validate_coherence(invalid)

    with pytest.raises(ir.DisplayIrError):
        ir.validate_shape({"aspect_ratio": -1})


def test_ir_maps_to_screen_layout_and_definition_json():
    widget = _widget_ir()
    layout = ir.to_screen_layout(widget)
    assert layout["id"] == "org.ozma.widget.cpu_temp"
    assert layout["widgets"][0]["type"] == "gauge"
    assert layout["widgets"][0]["metric"] == "org.ozma/node.cpu_temp"
    assert layout["widgets"][0]["field"] == 1
    assert layout["widgets"][0]["custom_badge"] == "new"
    assert layout["fields"][0]["key"] == "org.ozma/node.cpu_temp"
    assert layout["fields"][0]["type"] == "real"

    definition = ir.to_definition_json(widget, version=3)
    assert definition["id"] == "org.ozma.widget.cpu_temp"
    assert definition["version"] == 3
    assert definition["meta"]["ozma_ir"]["schema"] == ir.IR_SCHEMA
    assert definition["meta"]["ozma_ir"]["lossless"] is True
    assert (
        definition["meta"]["ozma_ir"]["widgets"][0]["source_hash"]
        == widget["source"]["source_hash"]
    )

    # The produced layout must still be accepted by the existing protocol
    # definition builder; this is the wire-compatibility check.
    import display_protocol as dp

    message = dp.build_definition(
        definition["layout"], definition["id"], definition["version"]
    )
    assert message.id == definition["id"]
    assert [field.key for field in message.fields] == ["org.ozma/node.cpu_temp"]
    assert [action.key for action in message.actions] == [
        "org.ozma.scenario.activate"
    ]


def test_from_screen_layout_preserves_unknown_fields():
    layout = {
        "id": "screen.one",
        "name": "Screen One",
        "width": 800,
        "height": 480,
        "background": "#0a0a0f",
        "refresh_hz": 5,
        "theme_hint": "custom-theme",
        "widgets": [
            {
                "type": "bar",
                "x": 1,
                "y": 2,
                "w": 100,
                "h": 10,
                "metric": "node.ram",
                "custom_flag": {"vendor": "example"},
                "text": "{node.ram}",
            }
        ],
    }

    widget = ir.from_screen_layout(layout)
    ir.validate_ir(widget)
    reserved = widget["extensions"]["screen_layout"]
    assert reserved["layout"]["theme_hint"] == "custom-theme"
    assert reserved["layout"]["background"] == "#0a0a0f"
    assert reserved["widget"]["custom_flag"] == {"vendor": "example"}
    assert reserved["widget"]["text"] == "{node.ram}"

    restored = ir.to_screen_layout(widget)
    assert restored["name"] == "Screen One"
    assert restored["background"] == "#0a0a0f"
    assert restored["refresh_hz"] == 5
    assert restored["theme_hint"] == "custom-theme"
    assert restored["widgets"][0]["custom_flag"] == {"vendor": "example"}
    assert restored["widgets"][0]["text"] == "{node.ram}"


def test_screen_layout_round_trip_from_ir_preserves_extensions():
    widget = _widget_ir()
    restored = ir.from_screen_layout(ir.to_screen_layout(widget))
    ir.validate_ir(restored)
    assert restored["id"] == widget["id"]
    assert restored["kind"] == widget["kind"]
    assert restored["binding"]["metric"] == widget["binding"]["metric"]
    assert (
        restored["extensions"]["screen_layout"]["widget"]["custom_badge"] == "new"
    )
    layout = ir.to_screen_layout(restored)
    assert layout["widgets"][0]["custom_badge"] == "new"


def test_canonical_hash_is_order_independent_and_definition_file_required():
    left = {"z": 1, "a": [True, None, {"y": 1.0, "x": "Grüße"}]}
    right = {"a": [True, None, {"x": "Grüße", "y": 1}], "z": 1}
    assert ir.canonical_json(left) == ir.canonical_json(right)
    assert ir.canonical_hash(left) == ir.canonical_hash(right)
    assert ir.canonical_json({"b": 1.0, "a": -0.0}) == '{"a":0,"b":1}'
    assert ir.canonical_hash({"a": 1}) != ir.canonical_hash({"a": 2})


def test_text_widget_maps_to_label_and_report_shape():
    widget = _widget_ir()
    widget["kind"] = "text"
    widget["binding"].pop("value_type")
    layout = ir.to_screen_layout(widget)
    assert layout["widgets"][0]["type"] == "label"
    assert layout["fields"][0]["type"] == "string"

    report = ir.require_export_report(_translation_report(), "browser_svg")
    assert report["direction"] == "export"
    assert report["fidelity"] in ir.FIDELITIES
    assert isinstance(report["dropped"], list)
    assert isinstance(report["warnings"], list)
