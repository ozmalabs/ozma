# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Structural performance-budget smoke tests (WGT-084).

Timing assertions are intentionally loose.  The deterministic structural
budgets (signed-index gzip bytes and Adaptive Card export bytes) carry the real
regression signal; the wall-clock ceilings only catch pathological stalls.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pytest

pytestmark = pytest.mark.unit

_ROOT = Path(__file__).resolve().parents[3]
_SCRIPT = _ROOT / "scripts" / "display_benchmarks.py"
_WORKLOADS = (
    "canonical_hash_validate",
    "asset_index_search",
    "ir_to_plan",
    "ir_to_lvgl",
    "adaptive_card_export",
    "font_match",
    "subset_report_planning",
)


@pytest.fixture(scope="module")
def quick_report() -> dict:
    completed = subprocess.run(
        [sys.executable, str(_SCRIPT), "--quick", "--json"],
        cwd=_ROOT,
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    assert completed.returncode == 0, (
        f"benchmark exited {completed.returncode}\n"
        f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
    )
    return json.loads(completed.stdout)


def test_quick_benchmark_json_shape_and_exit_code(quick_report: dict):
    assert quick_report["schema"] == "org.ozma.display-benchmarks/1"
    assert quick_report["mode"] == "quick"
    assert quick_report["runs"] > 0
    assert tuple(quick_report["workload_order"]) == _WORKLOADS
    assert quick_report["exit_code"] == 0
    assert quick_report["ok"] is True

    for name in _WORKLOADS:
        result = quick_report["workloads"][name]
        assert result["status"] in {"ok", "skipped"}, result
        if result["status"] == "skipped":
            # Only the Node-backed workload may be skipped (when node is absent).
            assert name == "ir_to_plan"
            assert result["metrics"].get("skipped") is True
            continue
        assert result["runs"] > 0
        assert result["hard"] is False
        assert 0.0 <= result["p50_ms"] <= result["p95_ms"] <= result["max_ms"]
        assert result["wall_ms"] >= result["max_ms"]
        # Very loose anti-stall ceiling; structural checks below are the real gate.
        assert result["p95_ms"] < 30_000, f"{name} stalled: {result['p95_ms']}ms"


def test_quick_benchmark_structural_budgets(quick_report: dict):
    index = quick_report["structural"]["signed_index_gzip_bytes"]
    assert index["limit"] == 2 * 1024 * 1024
    assert index["value"] > 0
    assert index["value"] <= index["limit"]
    assert index["ok"] is True
    assert quick_report["workloads"]["asset_index_search"]["metrics"]["entries"] == 10_000

    card = quick_report["structural"]["adaptive_card_bytes"]
    assert card["limit"] == 1024 * 1024
    assert 0 < card["value"] <= card["limit"]
    assert card["ok"] is True

    lvgl = quick_report["workloads"]["ir_to_lvgl"]["metrics"]
    assert lvgl["widgets"] == 9
    assert lvgl["source_bytes"] > 0
    assert lvgl["header_bytes"] > 0

    fonts = quick_report["workloads"]["font_match"]["metrics"]
    assert fonts["candidates"] > 0
    assert fonts["matches"] > 0

    planning = quick_report["workloads"]["subset_report_planning"]["metrics"]
    assert planning["physical_export"] is False
    assert planning["actions"] == ["exact", "subset"]
