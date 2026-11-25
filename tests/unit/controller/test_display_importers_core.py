# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Lossless importer tests for AIDA64, Rainmeter, and Home Assistant."""

from __future__ import annotations

import hashlib
import io
import json
import zipfile
from pathlib import Path

import pytest

import display_ir as ir
from display_importers import aida64
from display_importers.aida64 import AIDA64Importer, Aida64Importer
from display_importers.base import ImportError
from display_importers.home_assistant import HomeAssistantImporter
from display_importers.rainmeter import RainmeterImporter

pytestmark = pytest.mark.unit

_FIXTURES = Path(__file__).resolve().parents[2] / "fixtures" / "display_imports"
_UNSAFE_FIXTURES = {"ha_unsafe.yaml"}


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _importer_for(name: str):
    if name.startswith("aida64_"):
        return Aida64Importer()
    if name.startswith("rainmeter_"):
        return RainmeterImporter()
    if name.startswith("ha_"):
        return HomeAssistantImporter()
    raise AssertionError(f"no importer for fixture {name}")


def _fixture_paths() -> list[Path]:
    paths: list[Path] = []
    for prefix in ("aida64_", "rainmeter_", "ha_"):
        paths.extend(sorted(_FIXTURES.glob(f"{prefix}*")))
    return [path for path in paths if path.name not in _UNSAFE_FIXTURES]


def _import_fixture(name: str):
    data = (_FIXTURES / name).read_bytes()
    return _importer_for(name).import_source(name, data), data


def _asset_by_path(result, path: str):
    return next(asset for asset in result.assets if asset.path == path)


def _zip_bytes(entries: list[tuple[str, bytes]]) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in entries:
            archive.writestr(name, data)
    return buffer.getvalue()


def _symlink_zip_bytes(name: str) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        info = zipfile.ZipInfo(name)
        info.external_attr = 0o120777 << 16
        archive.writestr(info, b"")
    return buffer.getvalue()


@pytest.mark.parametrize("path", _fixture_paths(), ids=lambda path: path.name)
def test_lossless_invariants_for_every_fixture(path):
    data = path.read_bytes()
    result = _importer_for(path.name).import_source(path.name, data)

    assert result.lossless is True
    assert result.source_capture["lossless"] is True
    assert isinstance(result.source_capture["captured"], list)
    assert result.source_capture["source_hash"] == _sha256(data)
    assert result.source_document["source_hash"] == _sha256(data)
    assert result.raw == data

    exported = result.to_dict(include_bytes=True)
    assert exported["lossless"] is True
    assert exported["raw"] == data
    assert exported["source"]["source_hash"] == _sha256(data)
    assert exported["source_capture"]["source_hash"] == _sha256(data)

    original = result.source_document["original"]
    assert original["sha256"] == _sha256(data)
    assert _asset_by_path(result, original["path"]).data == data

    for asset in result.assets:
        ref = asset.to_asset_ref()
        assert ref["sha256"] == _sha256(asset.data)
        assert ref["size"] == len(asset.data)
        if ref["kind"] in {"script"}:
            assert ref["executable"] is False

    # Every widget validates against the shared IR contract and points at the
    # same authoritative source bytes.
    result.validate()
    for widget in result.widgets:
        ir.validate_ir(widget)
        assert widget["binding"]["metric"]
        assert widget["source"]["source_hash"] == _sha256(data)


def test_aida64_alias_and_detection():
    assert AIDA64Importer is Aida64Importer
    importer = Aida64Importer()
    assert importer.detect("panel.rslcd", b"<panel/>")
    assert importer.detect("panel.sensorpanel", b"{}")
    assert importer.sniff(b"PK\x03\x04rest")
    assert not importer.sniff(b"")


def test_aida64_xml_payload_maps_elements_and_retains_unknown_fields():
    result, _ = _import_fixture("aida64_sensorpanel.xml")
    kinds = {widget["kind"] for widget in result.widgets}
    assert {"gauge", "bar", "text", "number"} <= kinds

    payload = result.source_document["parsed"]["payloads"][0]
    assert payload["kind"] == "xml"
    assert payload["document"]["attributes"]["custom_root"] == "keep-me"

    gauge = next(widget for widget in result.widgets if widget["kind"] == "gauge")
    assert gauge["binding"]["metric"] == "TCPU"
    assert gauge["binding"]["unit"] == "C"
    assert gauge["domain"] == {"min": 0.0, "max": 100.0}
    assert gauge["layout"]["width"] == 120.0
    status = gauge["extensions"]["aida64"]
    assert status["attributes"]["custom_attr"] == "opaque"
    assert status["element"]["attributes"]["custom_attr"] == "opaque"

    # Embedded scripts are retained as non-executable assets, never run.
    script_assets = [asset for asset in result.assets if asset.kind == "script"]
    assert script_assets
    assert script_assets[0].executable is False
    assert b"never run" in script_assets[0].data
    assert result.unknown["scripts"]


def test_aida64_json_payload_maps_elements_and_retains_unknown_document():
    result, _ = _import_fixture("aida64_panel.json")
    kinds = [widget["kind"] for widget in result.widgets]
    assert kinds == ["gauge", "bar", "text", "number"]
    metrics = [widget["binding"]["metric"] for widget in result.widgets]
    assert metrics == ["TCPU", "SMEMUTI", "time", "SGPU1UTI"]

    document = result.source_document["parsed"]["payloads"][0]["document"]
    assert document["unknown_vendor"] == {"a": 1}
    sensor = result.widgets[-1]
    assert sensor["extensions"]["aida64"]["attributes"]["vendor_x"] == 1


def test_aida64_html_payload_retains_scripts_and_styles_opaque():
    result, _ = _import_fixture("aida64_remote.html")
    assert [widget["binding"]["metric"] for widget in result.widgets] == [
        "Simple1",
        "Simple2",
    ]
    script_assets = [asset for asset in result.assets if asset.kind == "script"]
    style_assets = [asset for asset in result.assets if asset.kind == "theme"]
    assert script_assets and style_assets
    assert b"window.bad" in script_assets[0].data
    assert b".gauge" in style_assets[0].data
    assert all(asset.executable is False for asset in script_assets + style_assets)
    assert result.unknown["scripts"]
    assert result.unknown["styles"]


def test_aida64_zip_members_are_retained_as_assets():
    result, data = _import_fixture("aida64_panel.zip")
    assert result.format == "aida64_rslcd"
    paths = {asset.path for asset in result.assets}
    assert {"panel.xml", "scripts/anim.js", "styles/theme.css", "images/bg.png"} <= paths
    script = _asset_by_path(result, "scripts/anim.js")
    assert script.kind == "script"
    assert script.executable is False
    assert script.data == b"window.animation = true;\n"
    assert result.source_document["parsed"]["classification"] == "zip"
    assert result.source_document["parsed"]["payloads"][0]["path"] == "panel.xml"
    assert {widget["kind"] for widget in result.widgets} == {"gauge", "bar"}
    # The original archive itself is recoverable byte-for-byte.
    assert _asset_by_path(result, "aida64_panel.zip").data == data


@pytest.mark.parametrize(
    "member",
    ["../evil.xml", "safe/../../evil.xml", "/etc/passwd", "C:\\evil.xml"],
)
def test_aida64_zip_rejects_path_traversal_and_absolute_members(member):
    with pytest.raises(ImportError):
        Aida64Importer().import_source("bad.rslcd", _zip_bytes([(member, b"x")]))


def test_aida64_zip_rejects_symlinks():
    with pytest.raises(ImportError):
        Aida64Importer().import_source(
            "bad.rslcd", _symlink_zip_bytes("link.xml")
        )


def test_aida64_zip_rejects_oversize_members(monkeypatch):
    monkeypatch.setattr(aida64, "MAX_MEMBER_SIZE", 4)
    with pytest.raises(ImportError):
        Aida64Importer().import_source(
            "bad.rslcd", _zip_bytes([("panel.xml", b"0123456789")])
        )


def test_aida64_unparseable_payload_is_opaque_with_warnings():
    result = Aida64Importer().import_source("broken.rslcd", b"not xml json or html")
    assert result.widgets == []
    assert result.warnings
    assert result.unknown["opaque"]
    assert result.source_document["parsed"]["classification"] == "opaque"
    assert result.lossless is True


def test_rainmeter_maps_measures_meters_bounds_and_unknown_keys():
    result, data = _import_fixture("rainmeter_skin.ini")
    by_section = {
        widget["extensions"]["rainmeter"]["section"]: widget for widget in result.widgets
    }

    bar = by_section["MeterRAMBar"]
    assert bar["kind"] == "bar"
    assert bar["binding"]["metric"] == "MeasureRAM"
    assert bar["binding"]["source"] == "rainmeter"
    assert bar["domain"] == {"min": 0.0, "max": 100.0}
    assert bar["layout"] == {"x": 10.0, "y": 60.0, "width": 200.0, "height": 20.0}
    assert bar["style"]["tokens"]["barcolor"] == "#00ff00"
    assert "UnknownMeterKey" not in bar["extensions"]["rainmeter"]["config"]
    assert bar["extensions"]["rainmeter"]["asset_references"] == [
        {"section": "MeterRAMBar", "key": "BarImage", "value": "images/bar.png"}
    ]

    string = by_section["MeterCPUString"]
    assert string["kind"] == "text"
    assert string["label"] == "CPU: %1%"
    assert string["typography"]["size_px"] == 12.0
    assert string["style"]["tokens"]["fontcolor"] == "#ffffff"
    assert "UnknownMeterKey" in string["extensions"]["rainmeter"]["config"]
    assert string["extensions"]["rainmeter"]["unknown_keys"] == ["UnknownMeterKey"]

    assert by_section["MeterHistory"]["kind"] == "sparkline"
    assert by_section["MeterHistogram"]["kind"] == "chart"
    assert by_section["MeterRing"]["kind"] == "gauge"
    assert by_section["MeterIcon"]["kind"] == "icon"

    # A measure not referenced by a meter still maps to a widget.
    assert by_section["MeasureTime"]["kind"] == "text"
    assert by_section["MeasureTime"]["extensions"]["rainmeter"]["unknown_keys"] == [
        "UnknownMeasureKey"
    ]
    assert (
        result.source_document["parsed"]["sections"]["MeasureCPU"]["UnknownMeasureKey"]
        == "keep-second"
    )
    duplicate_values = [
        entry["value"]
        for entry in result.source_document["parsed"]["raw_sections"]["MeasureCPU"]
        if entry["key"] == "UnknownMeasureKey"
    ]
    assert duplicate_values == ["keep", "keep-second"]

    # Script/plugin references never execute and are retained.
    scripts = result.unknown["scripts"]
    assert any(script["section"] == "MeasureScript" for script in scripts)
    assert scripts[0]["executable"] is False
    assert scripts[0]["script_file"].endswith("test.lua")
    assert "images/bar.png" in json.dumps(result.unknown["asset_references"])

    # Every section/key survives verbatim.
    parsed = result.source_document["parsed"]
    assert parsed["raw_text"] == data.decode("utf-8-sig")
    assert parsed["sections"]["MeasureCPU"]["UnknownMeasureKey"] == "keep-second"
    assert parsed["raw_sections"]["MeterCPUString"]


def test_rainmeter_rejects_invalid_ini():
    with pytest.raises(ImportError):
        RainmeterImporter().import_source("broken.ini", b"this is not ini")


def test_home_assistant_maps_cards_and_retains_opaque_content():
    result, _ = _import_fixture("ha_dashboard.yaml")
    by_type = {
        widget["extensions"]["home_assistant"]["type"]: widget
        for widget in result.widgets
    }

    gauge = by_type["gauge"]
    assert gauge["kind"] == "gauge"
    assert gauge["binding"]["metric"] == "sensor.cpu_temperature"
    assert gauge["binding"]["source"] == "home_assistant"
    assert gauge["binding"]["unit"] == "°C"
    assert gauge["domain"] == {"min": 0.0, "max": 100.0}
    assert gauge["extensions"]["home_assistant"]["unknown_keys"] == ["vendor_extra"]
    assert gauge["extensions"]["home_assistant"]["card"]["vendor_extra"] == "keep"
    assert gauge["actions"][0]["key"] == "home_assistant.tap_action"

    history = by_type["history-graph"]
    assert history["kind"] == "chart"
    assert history["history"]["window_seconds"] == 24 * 3600
    assert history["extensions"]["home_assistant"]["card"]["entities"][1] == {
        "entity": "sensor.memory_usage",
        "name": "Memory",
    }

    markdown = by_type["markdown"]
    assert markdown["kind"] == "text"
    assert "<script>alert(1)</script>" in markdown["extensions"]["home_assistant"]["card"]["content"]
    assert "custom: kept" in markdown["extensions"]["home_assistant"]["card"]["content"]

    assert by_type["button"]["kind"] == "status"
    assert by_type["button"]["extensions"]["home_assistant"]["tap_action"] == {
        "action": "toggle"
    }
    assert by_type["picture-entity"]["kind"] == "icon"
    assert by_type["entity"]["binding"]["metric"] == "sensor.gpu_temp"

    custom = by_type["custom:mini-graph-card"]
    assert custom["kind"] == "custom"
    assert custom["extensions"]["home_assistant"]["opaque"] is True
    assert custom["extensions"]["home_assistant"]["card"]["custom_option"] == "opaque"
    assert result.unknown["custom_cards"]


def test_home_assistant_custom_card_is_opaque():
    result, _ = _import_fixture("ha_custom_card.yaml")
    assert len(result.widgets) == 1
    widget = result.widgets[0]
    assert widget["kind"] == "custom"
    assert widget["extensions"]["home_assistant"]["custom_card"] is True
    assert widget["extensions"]["home_assistant"]["card"]["custom_option"] == {
        "deep": True
    }
    assert result.warnings


def test_home_assistant_rejects_unsafe_yaml():
    data = (_FIXTURES / "ha_unsafe.yaml").read_bytes()
    with pytest.raises(ImportError):
        HomeAssistantImporter().import_source("ha_unsafe.yaml", data)

    with pytest.raises(ImportError):
        HomeAssistantImporter().import_source(
            "secret.yaml", b"type: entities\nentities: !secret token\n"
        )


def test_home_assistant_rejects_non_card_documents():
    with pytest.raises(ImportError):
        HomeAssistantImporter().import_source("scalar.yaml", b"just a string\n")
    with pytest.raises(ImportError):
        HomeAssistantImporter().import_source("empty.yaml", b"")


def test_home_assistant_accepts_card_list():
    card_list = b"- type: sensor\n  entity: sensor.a\n- type: sensor\n  entity: sensor.b\n"
    result = HomeAssistantImporter().import_source("cards.yaml", card_list)
    assert [widget["binding"]["metric"] for widget in result.widgets] == [
        "sensor.a",
        "sensor.b",
    ]
    assert all(widget["kind"] == "number" for widget in result.widgets)
