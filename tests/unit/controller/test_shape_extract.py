# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for the controller-side shape, vibe, and coherence extractors."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import shape_extract as sx

pytestmark = pytest.mark.unit

_ROOT = Path(__file__).resolve().parents[3]
_WIDGET = _ROOT / "tests" / "fixtures" / "display_ir" / "widget_ir.json"


def _widget() -> dict:
    return json.loads(_WIDGET.read_text(encoding="utf-8"))


def _profile(value: dict) -> dict:
    return value


def _item(identifier: str, vibe: dict, shape: dict | None = None) -> dict:
    item = {"id": identifier, "vibe": vibe}
    if shape is not None:
        item["shape"] = shape
    return item


_SVG = """<svg viewBox="0 0 200 100" width="200" height="100">
    <rect x="0" y="0" width="100" height="50" rx="10" ry="10" fill="#ff0000"
          stroke="rgb(0, 128, 255)" stroke-width="4" />
    <path d="M 0 0 L 10 10 A 5 5 0 1 0 20 20 A 5 5 0 0 1 30 30 Z" />
    <circle cx="50" cy="50" r="20" fill="#abc" />
    <text font-family="'Inter', sans-serif" fill="rgba(0,0,0,0.5)">x</text>
    <animate attributeName="opacity" />
</svg>"""

_CSS = """
    .gauge {
        width: 120px;
        height: 60px;
        border-radius: 50%;
        border-width: 2px;
        background: linear-gradient(#ff0000, #0000ff);
        color: rgb(0, 128, 0);
        font-family: "JetBrains Mono", monospace;
        animation: sweep 1s linear infinite;
    }
"""


def test_widget_geometry_is_extracted_without_overwriting_source_fields():
    widget = _widget()
    shape = sx.extract_shape(widget)
    assert shape["widget_kind"] == "gauge"
    assert shape["aspect_ratio"] == 1
    assert shape["corner_radius_ratio"] == 0.5
    assert shape["stroke_width_ratio"] == 0.04
    assert shape["tick_count"] == 11
    assert shape["segment_count"] == 20
    assert shape["symmetry"] == "radial"
    assert shape["arc_start_deg"] == 135
    assert shape["arc_end_deg"] == 405
    assert shape["arc_span_deg"] == 270.0
    assert shape["palette_size"] == 3
    assert shape["confidence"] > 0
    assert "widget_ir" in shape["provenance"]["methods"]
    assert "density" in shape["provenance"]["derived_fields"]

    widget["shape"] = {"aspect_ratio": 2.0, "corner_radius_ratio": 0.1}
    merged = sx.extract_shape(widget)
    assert merged["aspect_ratio"] == 2.0
    assert merged["corner_radius_ratio"] == 0.1
    assert "aspect_ratio" in merged["provenance"]["source_fields"]


def test_svg_shape_palette_fonts_and_animation_are_parsed():
    extracted = sx.extract_from_svg(_SVG)
    shape = extracted["shape"]
    assert shape["aspect_ratio"] == 2.0
    assert shape["orientation"] == "horizontal"
    assert shape["corner_radius_ratio"] == 0.1
    assert shape["stroke_width_ratio"] == 0.04
    assert shape["fill_ratio"] == 0.25
    assert shape["segment_count"] == 3
    assert shape["arc_span_deg"] == 360.0
    assert shape["symmetry"] == "radial"
    assert extracted["animated"] is True
    assert "#ff0000" in extracted["colors"]
    assert "#0080ff" in extracted["colors"]
    assert "Inter" in extracted["fonts"]


def test_css_shape_palette_gradient_and_fonts_are_parsed():
    extracted = sx.extract_from_css(_CSS)
    shape = extracted["shape"]
    assert shape["aspect_ratio"] == 2.0
    assert shape["corner_radius_ratio"] == 0.5
    assert shape["stroke_width_ratio"] == 0.0333
    assert extracted["animated"] is True
    assert "#ff0000" in extracted["colors"]
    assert "#0000ff" in extracted["colors"]
    assert "#008000" in extracted["colors"]
    assert extracted["gradients"] == ["linear-gradient"]
    assert "JetBrains Mono" in extracted["fonts"]


def test_malformed_and_oversized_inputs_are_rejected():
    with pytest.raises(sx.ShapeExtractError, match="must not be empty"):
        sx.extract_from_svg("")
    with pytest.raises(sx.ShapeExtractError, match="closing tag"):
        sx.extract_from_svg("<svg viewBox='0 0 10 10'>")
    with pytest.raises(sx.ShapeExtractError, match="arc flags"):
        sx.extract_from_svg("<svg viewBox='0 0 10 10'><path d='A 1 1 0 2 0 1 1'/></svg>")
    with pytest.raises(sx.ShapeExtractError, match="entities"):
        sx.extract_from_svg("<!ENTITY xxe SYSTEM 'file:///etc/passwd'><svg/>")
    with pytest.raises(sx.ShapeExtractError, match="must not be empty"):
        sx.extract_from_css("")
    with pytest.raises(sx.ShapeExtractError, match="unbalanced braces"):
        sx.extract_from_css(".a { color: red; ")
    with pytest.raises(sx.ShapeExtractError, match="unterminated string"):
        sx.extract_from_css('.a { color: "red; }')
    with pytest.raises(sx.ShapeExtractError, match="exceeds"):
        sx.extract_from_css("x" * (sx.MAX_TEXT_INPUT + 1))


def test_path_arc_flags_are_validated_and_counted():
    stats = sx.parse_svg_path("M0 0 A 5 5 0 1 0 10 10 A 5 5 0 0 1 20 20")
    assert stats == {
        "commands": 3,
        "segments": 2,
        "arcs": 2,
        "large_arc_flags": 1,
        "sweep_flags": 1,
    }
    with pytest.raises(sx.ShapeExtractError):
        sx.parse_svg_path("M0 0 A 5 5 0 2 0 10 10")
    with pytest.raises(sx.ShapeExtractError):
        sx.parse_svg_path("M0 0 L")


def test_vibe_layers_are_separate_and_user_wins_conflicts():
    source = _profile(
        {
            "taxonomy": "org.ozma.vibe/1",
            "keywords": ["rounded"],
            "form": ["rounded"],
            "provenance": {"source_keywords": ["rounded"], "methods": ["manifest"]},
        }
    )
    user = _profile(
        {
            "keywords": ["sharp", "not_in_taxonomy"],
            "form": ["sharp"],
            "provenance": {
                "user_keywords": ["sharp", "not_in_taxonomy"],
                "methods": ["human"],
            },
        }
    )
    derived = [
        {"keyword": "rounded", "field": "form", "confidence": 0.8, "method": "geometry"},
        {"keyword": "warm", "field": "color", "confidence": 0.7, "method": "palette"},
    ]
    merged = sx.merge_vibe_keywords(source, user, derived)
    assert "sharp" in merged["keywords"]
    assert "not_in_taxonomy" in merged["keywords"]
    assert "rounded" not in merged["keywords"]
    assert "rounded" not in merged["form"]
    assert merged["provenance"]["source_keywords"] == ["rounded"]
    assert merged["provenance"]["user_keywords"] == ["sharp", "not_in_taxonomy"]
    assert merged["provenance"]["derived_keywords"] == ["warm"]
    assert "geometry" in merged["provenance"]["methods"]
    assert len(merged["derived"]) == 1


def test_unknown_user_keywords_are_retained_verbatim():
    merged = sx.merge_vibe_keywords(
        None,
        {"keywords": ["haunted", "not_in_taxonomy"], "provenance": {"user_keywords": ["haunted", "not_in_taxonomy"]}},
        [],
    )
    assert merged["keywords"] == ["haunted", "not_in_taxonomy"]
    assert merged["provenance"]["user_keywords"] == ["haunted", "not_in_taxonomy"]


def test_derive_vibe_uses_palette_typography_geometry_and_motion():
    vibe = sx.derive_vibe(
        {
            "colors": ["#ff5500", "#ffcc00"],
            "fonts": ["JetBrains Mono"],
            "typography": {"weight": 700, "width": "condensed"},
            "shape": {
                "corner_radius_ratio": 0.5,
                "symmetry": "radial",
                "tick_count": 12,
                "density": 8.0,
                "animation": "sweep",
            },
            "references": ["aviation"],
        }
    )
    assert vibe["taxonomy"] == sx.VIBE_TAXONOMY
    assert "warm" in vibe["color"]
    assert "monospace" in vibe["typography"]
    assert "bold" in vibe["typography"]
    assert "condensed" in vibe["typography"]
    assert "rounded" in vibe["form"]
    assert "circular" in vibe["form"]
    assert vibe["density"] == "dense"
    assert "animated" in vibe["motion"]
    assert "sweep" in vibe["motion"]
    assert "aviation" in vibe["references"]
    assert vibe["provenance"]["source_keywords"] == []
    assert vibe["provenance"]["user_keywords"] == []
    assert vibe["provenance"]["derived_keywords"]
    assert all(entry["confidence"] > 0 for entry in vibe["derived"])


def test_coherence_single_and_empty_inputs_are_neutral():
    single = sx.coherence_for([_item("only", {"keywords": ["calm"], "color": ["warm"]})])
    assert single["score"] == 1.0
    assert single["confidence"] == 0.25
    assert single["dimensions"]["color"] == 1.0

    empty = sx.coherence_for([])
    assert empty == {
        "score": 1.0,
        "dimensions": {},
        "outliers": [],
        "conflicts": [],
        "suggestions": [],
        "confidence": 0.0,
    }


def test_coherence_outliers_conflicts_and_intentional_mixing():
    items = [
        _item(
            "a",
            {"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"},
        ),
        _item(
            "b",
            {"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"},
        ),
        _item(
            "c",
            {"color": ["cool"], "form": ["sharp"], "motion": ["animated"], "density": "dense"},
        ),
    ]
    report = sx.coherence_for(items)
    assert report["score"] < 1.0
    assert report["dimensions"]["color"] < 1.0
    assert report["outliers"] == ["c"]
    assert "rounded vs sharp" in report["conflicts"]
    assert "animated vs static" in report["conflicts"]

    overridden = sx.coherence_for(items, {"intentional_mixing": True})
    assert overridden["score"] == 1.0
    assert overridden["conflicts"] == []
    assert overridden["outliers"] == []
    assert "intentional mixing accepted" in overridden["suggestions"]

    accepted = sx.coherence_for(items, {"accepted_conflicts": ["rounded vs sharp"]})
    assert "rounded vs sharp" not in accepted["conflicts"]
    assert "animated vs static" in accepted["conflicts"]


def test_coherence_scores_are_bounded_and_stable():
    items = [
        _item("a", {"color": ["warm"], "form": ["rounded"]}),
        _item("b", {"color": ["cool"], "form": ["sharp"]}),
    ]
    first = json.dumps(sx.coherence_for(items), sort_keys=True)
    second = json.dumps(sx.coherence_for(items), sort_keys=True)
    assert first == second
    report = json.loads(first)
    assert 0.0 <= report["score"] <= 1.0
    assert 0.0 <= report["confidence"] <= 1.0
    assert all(0.0 <= value <= 1.0 for value in report["dimensions"].values())


def test_golden_json_matches_rust_fixture_for_derived_vibe():
    """This fixture is duplicated in ``extract.rs`` for parity."""
    vibe = sx.derive_vibe({"colors": ["#ff0000"], "shape": {"corner_radius_ratio": 0.0}})
    assert vibe == {
        "taxonomy": "org.ozma.vibe/1",
        "keywords": ["neon", "warm", "sharp"],
        "mood": [],
        "form": ["sharp"],
        "material": [],
        "color": ["neon", "warm"],
        "typography": [],
        "motion": [],
        "references": [],
        "ornament": [],
        "confidence": 0.8167,
        "provenance": {
            "source_keywords": [],
            "derived_keywords": ["neon", "warm", "sharp"],
            "user_keywords": [],
            "methods": ["geometry", "palette"],
        },
        "derived": [
            {
                "keyword": "neon",
                "field": "color",
                "confidence": 0.75,
                "method": "palette",
            },
            {
                "keyword": "warm",
                "field": "color",
                "confidence": 0.9,
                "method": "palette",
            },
            {
                "keyword": "sharp",
                "field": "form",
                "confidence": 0.8,
                "method": "geometry",
            },
        ],
    }


def test_golden_json_matches_rust_fixture_for_coherence():
    """This fixture is duplicated in ``extract.rs`` for parity."""
    items = [
        _item("a", {"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"}),
        _item("b", {"color": ["warm"], "form": ["rounded"], "motion": ["static"], "density": "sparse"}),
        _item("c", {"color": ["cool"], "form": ["sharp"], "motion": ["animated"], "density": "dense"}),
    ]
    assert sx.coherence_for(items) == {
        "score": 0.3333,
        "dimensions": {
            "form": 0.3333,
            "color": 0.3333,
            "density": 0.3333,
            "motion": 0.3333,
        },
        "outliers": ["c"],
        "conflicts": [
            "animated vs static",
            "cool vs warm",
            "dense vs sparse",
            "rounded vs sharp",
        ],
        "suggestions": [
            "resolve conflicting keywords",
            "review density",
            "review geometry",
            "review motion",
            "review palette",
        ],
        "confidence": 0.9,
    }


def test_extracted_results_round_trip_through_json():
    results = [
        sx.extract_shape(_widget()),
        sx.extract_from_svg(_SVG),
        sx.extract_from_css(_CSS),
        sx.derive_vibe({"colors": ["#ff0000"], "shape": {"corner_radius_ratio": 0.0}}),
        sx.coherence_for([_item("a", {"color": ["warm"]}), _item("b", {"color": ["warm"]})]),
    ]
    for result in results:
        assert json.loads(json.dumps(result)) == result
