# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Golden parity tests for display export targets (WGT-045/064/066).

Regenerate intentionally (test-only, guarded)::

    UPDATE_GOLDENS=1 python -m pytest \
        tests/unit/controller/test_display_golden.py -q

The regenerator rewrites only this test's files under
``tests/fixtures/display_golden/`` and merges its entries into ``hashes.json``.
A golden diff is a production-behaviour change: inspect it before updating.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path

import pytest

import display_ir as ir
from display_importers.adaptive_cards import (
    MAX_EXPORT_BYTES,
    AdaptiveCardError,
    export_adaptive_card,
)
from lvgl_export import LvglExportError, export_lvgl, export_lvgl_json

pytestmark = pytest.mark.unit

_ROOT = Path(__file__).resolve().parents[3]
_GOLDEN_DIR = _ROOT / "tests" / "fixtures" / "display_golden"
_SCHEMAS = _ROOT / "schemas" / "display"
_HASHES_FILE = _GOLDEN_DIR / "hashes.json"
_UPDATE_GOLDENS = os.environ.get("UPDATE_GOLDENS") == "1"
_OWNED_FILES = (
    "lvgl_export.c",
    "lvgl_export.h",
    "lvgl_export.json",
    "adaptive_card.json",
)


def _load_json(name: str) -> dict:
    return json.loads((_GOLDEN_DIR / name).read_text(encoding="utf-8"))


def _serialize_json(value: object) -> str:
    return json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def _serialize_text(value: str) -> str:
    return value if value.endswith("\n") else f"{value}\n"


def _write_golden(name: str, text: str) -> None:
    (_GOLDEN_DIR / name).write_text(text, encoding="utf-8")


def _first_diff(expected: str, actual: str) -> str:
    expected_lines = expected.splitlines()
    actual_lines = actual.splitlines()
    limit = max(len(expected_lines), len(actual_lines))
    for index in range(limit):
        left = expected_lines[index] if index < len(expected_lines) else None
        right = actual_lines[index] if index < len(actual_lines) else None
        if left == right:
            continue
        context = []
        for line in range(max(0, index - 2), min(limit, index + 3)):
            shown_left = expected_lines[line] if line < len(expected_lines) else None
            shown_right = actual_lines[line] if line < len(actual_lines) else None
            context.append(f"  {line + 1}: expected {shown_left!r}")
            context.append(f"  {line + 1}: actual   {shown_right!r}")
        return f"first difference at line {index + 1}\n" + "\n".join(context)
    return "no textual difference"


def _assert_golden(name: str, actual: str) -> None:
    if _UPDATE_GOLDENS:
        _write_golden(name, actual)
        return
    expected_path = _GOLDEN_DIR / name
    assert expected_path.exists(), f"missing golden {expected_path}"
    expected = expected_path.read_text(encoding="utf-8")
    assert actual == expected, (
        f"golden mismatch for {name}\n{_first_diff(expected, actual)}\n"
        "Regenerate intentionally with UPDATE_GOLDENS=1 pytest "
        "tests/unit/controller/test_display_golden.py"
    )


def _sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _c_float(value: float) -> str:
    text = f"{float(value):.3f}".rstrip("0").rstrip(".")
    if text in ("", "-0"):
        text = "0"
    if "." not in text:
        text = f"{text}.0"
    return f"{text}f"


_LAYOUT = _load_json("screen_layout.json")
_THEME = _load_json("theme.json")
_DATA = _load_json("data.json")
_OPTIONS = {"tokens": _THEME["tokens"]}
_LVGL = export_lvgl(_LAYOUT, _OPTIONS)
_LVGL_METADATA = export_lvgl_json(_LAYOUT, _OPTIONS)
_CARD_EXPORT = export_adaptive_card(_LAYOUT)


# ---------------------------------------------------------------------------
# Fixture validity
# ---------------------------------------------------------------------------


def test_canonical_screen_layout_converts_to_schema_valid_ir():
    widgets = ir.from_screen_layout_many(_LAYOUT)
    assert len(widgets) == len(_LAYOUT["widgets"])
    for widget in widgets:
        ir.validate_ir(widget)
        assert len(widget["source"]["source_hash"]) == 64

    jsonschema = pytest.importorskip("jsonschema")
    schema = json.loads((_SCHEMAS / "ir-v1.schema.json").read_text(encoding="utf-8"))
    jsonschema.Draft202012Validator.check_schema(schema)
    validator = jsonschema.Draft202012Validator(schema)
    for widget in widgets:
        errors = sorted(validator.iter_errors(widget), key=lambda error: list(error.path))
        assert not errors, errors

    hashed = ir.canonical_hash(_LAYOUT)
    assert len(hashed) == 64
    assert hashed == ir.canonical_hash(json.loads(json.dumps(_LAYOUT)))


# ---------------------------------------------------------------------------
# LVGL C/header/JSON goldens
# ---------------------------------------------------------------------------


def test_lvgl_c_header_and_json_goldens_match_exactly():
    _assert_golden("lvgl_export.c", _serialize_text(_LVGL["source"]))
    _assert_golden("lvgl_export.h", _serialize_text(_LVGL["header"]))
    _assert_golden("lvgl_export.json", _serialize_json(_LVGL_METADATA))
    assert _LVGL["metadata"] == _LVGL_METADATA


def test_lvgl_c_constants_match_ir_values():
    source = _LVGL["source"]
    header = _LVGL["header"]
    metadata = _LVGL_METADATA

    assert "#define OZMA_LVGL_WIDTH 1280" in source
    assert "#define OZMA_LVGL_HEIGHT 720" in source
    assert "#define OZMA_LVGL_BG 0x0b0d12" in source
    assert "#define OZMA_LVGL_WIDGET_COUNT 9" in header

    widgets = {widget["id"]: widget for widget in metadata["widgets"]}
    assert widgets["cpu_temp"]["value"] == 72.5
    assert widgets["cpu_temp"]["min"] == 0
    assert widgets["cpu_temp"]["max"] == 100
    assert widgets["gpu_history"]["points"] == 10

    assert "lv_arc_set_range(ozma_lvgl_cpu_temp_0, (int32_t)0.0f, (int32_t)100.0f);" in source
    assert "lv_arc_set_value(ozma_lvgl_cpu_temp_0, (int32_t)72.5f);" in source
    assert '{ 75.0f, "warning", lv_color_hex(0xf0b94a) },' in source
    assert '{ 90.0f, "critical", lv_color_hex(0xf26464) },' in source
    assert "lv_bar_set_value(ozma_lvgl_memory_1, (int32_t)63.0f, LV_ANIM_OFF);" in source
    assert "lv_bar_set_range(ozma_lvgl_memory_1, (int32_t)0.0f, (int32_t)100.0f);" in source
    assert '{ 80.0f, "warning", lv_color_hex(0xf0b94a) },' in source
    assert '{ 95.0f, "critical", lv_color_hex(0xf26464) },' in source
    assert "lv_label_set_text(ozma_lvgl_cpu_load_2, \"42%\");" in source

    # Every threshold in metadata is present verbatim in the generated C.
    for widget in metadata["widgets"]:
        for threshold in widget["thresholds"]:
            color = threshold["color"].replace("#", "0x")
            line = (
                f'{{ {_c_float(threshold["at"])}, "{threshold["severity"]}", '
                f"lv_color_hex({color}) }},"
            )
            assert line in source, f"missing C threshold {line} for {widget['id']}"

    report = metadata["translation_report"]
    assert report["direction"] == "export"
    assert report["target_format"] == "lvgl_c"
    assert report["fidelity"] == "approximate"
    assert len(metadata["source_hash"]) == 64
    assert len(metadata["ir_hash"]) == 64
    assert any("gauge tick marks" in item for item in report["approximated"])
    assert any("gauge animation/effects" in item for item in report["dropped"])


# ---------------------------------------------------------------------------
# Adaptive Card export golden and explicit report
# ---------------------------------------------------------------------------


def test_adaptive_card_golden_and_explicit_report():
    card = _CARD_EXPORT.card
    report = _CARD_EXPORT.report
    _assert_golden("adaptive_card.json", _serialize_json(_CARD_EXPORT.to_dict()))

    assert card["type"] == "AdaptiveCard"
    assert card["version"] == "1.5"
    assert len(card["body"]) == len(_LAYOUT["widgets"])
    assert report["direction"] == "export"
    assert report["target_format"] == "adaptive_card"
    assert report["fidelity"] == "approximate"

    approximated = set(report["approximated"])
    assert "cpu_temp: gauge exported as TextBlock" in approximated
    assert "memory: bar exported as TextBlock" in approximated
    assert "cpu_load: numeric value exported as TextBlock" in approximated
    assert "gpu_history: sparkline exported as TextBlock" in approximated
    assert "cooling: icon exported as TextBlock" in approximated
    assert "quick_stats: table exported as TextBlock" in approximated

    dropped = set(report["dropped"])
    assert "cpu_temp: gauge chart/gauge semantics" in dropped
    assert "memory: bar chart/gauge semantics" in dropped
    assert "gpu_history: sparkline chart/gauge semantics" in dropped
    assert "cooling: image source" in dropped

    encoded = json.dumps(card, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    assert len(encoded) <= MAX_EXPORT_BYTES
    assert len(encoded) <= 16_384, "golden card should stay far below the 1 MiB limit"


# ---------------------------------------------------------------------------
# Fail closed
# ---------------------------------------------------------------------------


def test_exporters_fail_closed_on_unknown_or_invalid_values():
    with pytest.raises(LvglExportError):
        export_lvgl(
            {
                "widgets": [
                    {
                        "type": "gauge",
                        "x": 0,
                        "y": 0,
                        "w": 10,
                        "h": 10,
                        "value": float("nan"),
                    }
                ]
            }
        )
    with pytest.raises(AdaptiveCardError):
        export_adaptive_card(_LAYOUT, {"version": "1.4"})
    with pytest.raises(AdaptiveCardError):
        export_adaptive_card(_LAYOUT, {"max_bytes": 32})
    with pytest.raises(ir.DisplayIrError):
        ir.validate_ir({"id": "bad", "kind": "hologram", "binding": {"metric": "x"}})


# ---------------------------------------------------------------------------
# Stable hashes
# ---------------------------------------------------------------------------


def test_golden_artifact_hashes_are_stable():
    hashes = (
        json.loads(_HASHES_FILE.read_text(encoding="utf-8"))
        if _HASHES_FILE.exists()
        else {}
    )
    if _UPDATE_GOLDENS:
        for name in _OWNED_FILES:
            hashes[name] = _sha256((_GOLDEN_DIR / name).read_text(encoding="utf-8"))
        _HASHES_FILE.write_text(
            json.dumps(hashes, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        return
    for name in _OWNED_FILES:
        assert name in hashes, f"hashes.json is missing {name}"
        actual = _sha256((_GOLDEN_DIR / name).read_text(encoding="utf-8"))
        assert actual == hashes[name], f"hash mismatch for {name}"
