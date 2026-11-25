#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Deterministic display-platform performance budgets (WGT-084).

Usage::

    python scripts/display_benchmarks.py --quick
    python scripts/display_benchmarks.py --full --json

Workloads use deterministic synthetic data and never touch the network:

* canonical IR validation + canonical hash;
* signed-index search over 10,000 entries, with a gzip size budget;
* IR -> render plan through the production Node adapter;
* IR -> LVGL C/header/metadata through ``controller/lvgl_export.py``;
* Adaptive Card export through ``controller/display_importers``;
* font matching over synthetic metadata (no font files required);
* subset/report planning (report only; no physical subsetting).

The plan §10 budgets are interpreted conservatively for CI.  Time budgets are
soft in ``--quick`` and hard in ``--full``; structural budgets (for example the
2 MiB gzip signed index) are always reported and are hard in ``--full``.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import math
import shutil
import subprocess
import sys
import time
from pathlib import Path

BENCHMARK_SCHEMA = "org.ozma.display-benchmarks/1"

ROOT = Path(__file__).resolve().parents[1]
CONTROLLER = ROOT / "controller"
GOLDEN_DIR = ROOT / "tests" / "fixtures" / "display_golden"
RENDERER = ROOT / "renderer" / "display_ir.js"

if str(CONTROLLER) not in sys.path:
    sys.path.insert(0, str(CONTROLLER))

INDEX_ENTRIES = 10_000
GZIP_INDEX_BUDGET = 2 * 1024 * 1024  # §10: signed index <= 2 MiB gzip for 10k packs
ADAPTIVE_CARD_BUDGET = 1024 * 1024

WORKLOAD_ORDER = (
    "canonical_hash_validate",
    "asset_index_search",
    "ir_to_plan",
    "ir_to_lvgl",
    "adaptive_card_export",
    "font_match",
    "subset_report_planning",
)

RUNS = {"quick": 3, "full": 7}

TIME_BUDGETS = {
    "quick": {
        "canonical_hash_validate": (3_000, "generous quick-mode ceiling"),
        "asset_index_search": (5_000, "generous quick-mode ceiling"),
        "ir_to_plan": (5_000, "generous quick-mode ceiling; includes Node startup"),
        "ir_to_lvgl": (3_000, "generous quick-mode ceiling"),
        "adaptive_card_export": (3_000, "generous quick-mode ceiling"),
        "font_match": (3_000, "generous quick-mode ceiling"),
        "subset_report_planning": (3_000, "generous quick-mode ceiling"),
    },
    "full": {
        "canonical_hash_validate": (500, "CI-friendly validation/hash ceiling"),
        "asset_index_search": (500, "CI-friendly 10k-entry search ceiling"),
        "ir_to_plan": (
            1_500,
            "§10 browser first render is 250 ms; includes Node process startup",
        ),
        "ir_to_lvgl": (1_000, "CI-friendly export ceiling; no direct §10 budget"),
        "adaptive_card_export": (1_000, "CI-friendly export ceiling"),
        "font_match": (1_000, "synthetic metadata matching; no physical fonts"),
        "subset_report_planning": (
            1_000,
            "report planning only; no physical subset",
        ),
    },
}


# ---------------------------------------------------------------------------
# Timing helpers
# ---------------------------------------------------------------------------


def _percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    if not ordered:
        return 0.0
    index = min(len(ordered) - 1, max(0, math.ceil(fraction * len(ordered)) - 1))
    return ordered[index]


def _measure(workload, runs: int) -> dict:
    times: list[float] = []
    metrics: dict = {}
    wall_start = time.perf_counter()
    for _ in range(runs):
        started = time.perf_counter()
        metrics = workload()
        times.append((time.perf_counter() - started) * 1_000.0)
    wall_ms = (time.perf_counter() - wall_start) * 1_000.0
    return {
        "runs": runs,
        "p50_ms": round(_percentile(times, 0.50), 3),
        "p95_ms": round(_percentile(times, 0.95), 3),
        "max_ms": round(max(times), 3),
        "wall_ms": round(wall_ms, 3),
        "metrics": metrics,
    }


# ---------------------------------------------------------------------------
# Deterministic synthetic corpora
# ---------------------------------------------------------------------------


def _load_golden_fixtures() -> tuple[dict, dict]:
    layout = json.loads((GOLDEN_DIR / "screen_layout.json").read_text(encoding="utf-8"))
    theme = json.loads((GOLDEN_DIR / "theme.json").read_text(encoding="utf-8"))
    return layout, theme


def _ir_payload(layout: dict, count: int) -> list[dict]:
    import display_ir as ir

    base = ir.from_screen_layout_many(layout)
    widgets: list[dict] = []
    for index in range(count):
        widget = json.loads(json.dumps(base[index % len(base)]))
        widget["id"] = f"{widget['id']}#{index}"
        widgets.append(widget)
    return widgets


def _index_corpus(count: int) -> tuple[list[tuple[str, str]], bytes]:
    entries = []
    vibes = ("precise", "calm", "retro", "instrumentation", "haunted")
    shapes = ("gauge", "bar", "chart", "radial", "segmented")
    for index in range(count):
        entry = {
            "id": f"org.ozma.pack.{index:05d}",
            "type": ("widget_pack", "theme_pack", "description_pack")[index % 3],
            "name": f"Synthetic Pack {index:05d}",
            "version": "1.0.0",
            "license": "MIT",
            "tags": [f"tag-{index % 17}", "synthetic", "offline"],
            "vibe": {"keywords": [vibes[index % len(vibes)], "nocturnal"]},
            "shape": {"widget_kind": shapes[index % len(shapes)]},
            "coherence": {"score": round((index % 100) / 100.0, 2)},
            "summary": (
                "Deterministic benchmark entry with no network or binary assets; "
                "used to size and search the signed 10k-pack index."
            ),
        }
        entries.append(entry)
    raw = json.dumps(entries, sort_keys=True, separators=(",", ":")).encode("utf-8")
    compressed = gzip.compress(raw, compresslevel=9, mtime=0)
    corpus = []
    for entry in entries:
        text = " ".join(
            [
                entry["id"],
                entry["type"],
                entry["name"],
                entry["license"],
                " ".join(entry["tags"]),
                " ".join(entry["vibe"]["keywords"]),
                entry["shape"]["widget_kind"],
            ]
        ).lower()
        corpus.append((entry["id"], text))
    return corpus, compressed


def _font_candidates(count: int) -> list[dict]:
    candidates = []
    classifications = ("sans", "serif", "mono", "display")
    for index in range(count):
        family = f"Bench {index:03d}"
        candidates.append(
            {
                "id": f"font-bench-{index:03d}",
                "family": family,
                "style": "Regular",
                "version": "1.000",
                "format": "ttf",
                "sha256": hashlib.sha256(family.encode("utf-8")).hexdigest(),
                "size_bytes": 16_384 + index,
                "source": "benchmark",
                "license": "OFL-1.1",
                "embedding_allowed": True,
                "subset_allowed": True,
                "units_per_em": 1_000,
                "ascent": 800,
                "descent": -200,
                "x_height": 500,
                "cap_height": 700,
                "average_advance": 500.0,
                "glyph_count": 500,
                "classification": classifications[index % len(classifications)],
                "weight": 400,
                "width": 5,
                "width_name": "normal",
                "slant": "upright",
                "scripts": ["latin"],
                "features": ["tabular_nums"],
                "panose": "2 11 5 3 2 2 3 2 2 4",
            }
        )
    return candidates


# ---------------------------------------------------------------------------
# Workloads
# ---------------------------------------------------------------------------


def _workloads(mode: str) -> dict:
    layout, theme = _load_golden_fixtures()
    options = {"tokens": theme["tokens"]}
    ir_count = 9 if mode == "quick" else 256
    widgets = _ir_payload(layout, ir_count)
    corpus, compressed = _index_corpus(INDEX_ENTRIES)
    node = shutil.which("node")

    def canonical_hash_validate() -> dict:
        import display_ir as ir

        for widget in widgets:
            ir.validate_ir(widget)
        digest = ir.canonical_hash({"schema": ir.IR_SCHEMA, "widgets": widgets})
        return {"widgets": len(widgets), "hash": digest}

    def asset_index_search() -> dict:
        tokens = ("gauge", "tag-7", "vibe-12")
        matches = 0
        for token in tokens:
            matches += sum(1 for _, text in corpus if token in text)
        return {
            "entries": len(corpus),
            "tokens": len(tokens),
            "matches": matches,
            "gzip_bytes": len(compressed),
        }

    node_script = r"""
const fs = require('fs');
const displayIr = require(process.argv[1]);
const input = JSON.parse(fs.readFileSync(0, 'utf8'));
const plan = displayIr.irToRenderPlan(input.layout, input.options);
process.stdout.write(JSON.stringify({
  schema: plan.schema,
  width: plan.width,
  height: plan.height,
  ops: plan.ops.length,
  warnings: plan.warnings.length
}));
"""
    node_input = json.dumps({"layout": layout, "options": {"tokens": theme["tokens"]}})

    def ir_to_plan() -> dict:
        if not node:
            return {"skipped": True, "reason": "node executable not found"}
        completed = subprocess.run(
            [node, "-e", node_script, str(RENDERER)],
            input=node_input,
            text=True,
            capture_output=True,
            check=False,
            timeout=60,
        )
        if completed.returncode != 0:
            raise RuntimeError(completed.stderr.strip() or "node render failed")
        plan = json.loads(completed.stdout)
        return {
            "schema": plan["schema"],
            "width": plan["width"],
            "height": plan["height"],
            "ops": plan["ops"],
            "warnings": plan["warnings"],
        }

    def ir_to_lvgl() -> dict:
        from lvgl_export import export_lvgl

        result = export_lvgl(layout, options)
        metadata = result["metadata"]
        return {
            "source_bytes": len(result["source"].encode("utf-8")),
            "header_bytes": len(result["header"].encode("utf-8")),
            "widgets": metadata["counts"]["widgets"],
            "thresholds": metadata["counts"]["thresholds"],
        }

    def adaptive_card_export() -> dict:
        from display_importers.adaptive_cards import export_adaptive_card

        result = export_adaptive_card(layout)
        encoded = json.dumps(
            result.card, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        return {
            "body_elements": len(result.card.get("body", [])),
            "card_bytes": len(encoded),
            "approximated": len(result.report["approximated"]),
            "dropped": len(result.report["dropped"]),
        }

    candidates = _font_candidates(32 if mode == "quick" else 256)

    def font_match() -> dict:
        import font_translation as ft

        intent = ft.FontIntent.new("body", "sans")
        intent.scripts = ["latin"]
        intent.features = ["tnum"]
        matches = ft.match_fonts(intent, candidates)
        return {
            "candidates": len(candidates),
            "matches": len(matches),
            "top": matches[0].font.family if matches else "",
        }

    def subset_report_planning() -> dict:
        import font_translation as ft

        intent = ft.FontIntent.new("body", "sans")
        intent.scripts = ["latin"]
        original = dict(candidates[0])
        targets = (
            {"target": "browser", "formats": ["ttf"], "max_bytes": 50_000},
            {"target": "lvgl", "formats": ["ttf"], "max_bytes": 512},
        )
        actions = [
            ft.plan_export(intent, original, candidates[:8], target).action
            for target in targets
        ]
        return {"actions": actions, "physical_export": False}

    return {
        "canonical_hash_validate": canonical_hash_validate,
        "asset_index_search": asset_index_search,
        "ir_to_plan": ir_to_plan,
        "ir_to_lvgl": ir_to_lvgl,
        "adaptive_card_export": adaptive_card_export,
        "font_match": font_match,
        "subset_report_planning": subset_report_planning,
    }


# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------


def run_benchmarks(mode: str) -> dict:
    runs = RUNS[mode]
    workloads = _workloads(mode)
    results: dict = {}

    for name in WORKLOAD_ORDER:
        limit_ms, label = TIME_BUDGETS[mode][name]
        hard = mode == "full"
        try:
            measured = _measure(workloads[name], runs)
            skipped = bool(measured["metrics"].get("skipped"))
            if skipped:
                status = "skipped"
                ok = True
            else:
                status = "ok"
                ok = measured["p95_ms"] <= limit_ms
        except Exception as error:  # noqa: BLE001 - report, do not hide workload errors
            measured = {
                "runs": 0,
                "p50_ms": 0.0,
                "p95_ms": 0.0,
                "max_ms": 0.0,
                "wall_ms": 0.0,
                "metrics": {},
            }
            status = "error"
            ok = False
            measured["error"] = f"{type(error).__name__}: {error}"
        measured.update(
            {
                "status": status,
                "ok": ok,
                "budget_ms": limit_ms,
                "budget_label": label,
                "hard": hard and not measured["metrics"].get("skipped"),
            }
        )
        results[name] = measured

    index_metric = results["asset_index_search"]["metrics"]
    gzip_bytes = int(index_metric.get("gzip_bytes", 0))
    card_bytes = int(results["adaptive_card_export"]["metrics"].get("card_bytes", 0))
    structural = {
        "signed_index_gzip_bytes": {
            "value": gzip_bytes,
            "limit": GZIP_INDEX_BUDGET,
            "ok": gzip_bytes <= GZIP_INDEX_BUDGET,
            "label": "§10 signed index <= 2 MiB gzip for 10k packs",
        },
        "adaptive_card_bytes": {
            "value": card_bytes,
            "limit": ADAPTIVE_CARD_BUDGET,
            "ok": card_bytes <= ADAPTIVE_CARD_BUDGET,
            "label": "Adaptive Card export stays within the 1 MiB export bound",
        },
    }

    errors = any(result["status"] == "error" for result in results.values())
    structural_ok = all(check["ok"] for check in structural.values())
    hard_time_failure = any(
        result["hard"] and not result["ok"] for result in results.values()
    )
    exit_code = 0
    if errors or (mode == "full" and (hard_time_failure or not structural_ok)):
        exit_code = 1

    return {
        "schema": BENCHMARK_SCHEMA,
        "mode": mode,
        "runs": runs,
        "workload_order": list(WORKLOAD_ORDER),
        "workloads": results,
        "structural": structural,
        "ok": not errors and (not hard_time_failure or mode != "full") and structural_ok,
        "exit_code": exit_code,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode_group = parser.add_mutually_exclusive_group()
    mode_group.add_argument(
        "--quick", action="store_true", help="generous soft budgets (default)"
    )
    mode_group.add_argument("--full", action="store_true", help="hard CI budgets")
    parser.add_argument("--json", action="store_true", help="emit the report as JSON")
    args = parser.parse_args(argv)
    mode = "full" if args.full else "quick"

    report = run_benchmarks(mode)
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(f"display benchmarks: {mode} ({report['runs']} runs)")
        for name in WORKLOAD_ORDER:
            result = report["workloads"][name]
            print(
                f"  {name:24s} {result['status']:7s} "
                f"p50={result['p50_ms']:8.3f}ms p95={result['p95_ms']:8.3f}ms "
                f"max={result['max_ms']:8.3f}ms budget={result['budget_ms']}ms"
            )
        for name, check in report["structural"].items():
            print(
                f"  {name:24s} {check['value']} <= {check['limit']} "
                f"({'ok' if check['ok'] else 'FAIL'})"
            )
        print(f"exit: {report['exit_code']}")
    return int(report["exit_code"])


if __name__ == "__main__":
    raise SystemExit(main())
