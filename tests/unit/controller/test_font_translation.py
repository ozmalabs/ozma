# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for controller-side font metadata, matching, and export planning."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

import font_translation as ft

pytestmark = pytest.mark.unit

_SCHEMAS = Path(__file__).resolve().parents[3] / "schemas" / "display"
_FONT_ROOTS = (
    Path("/usr/share/fonts/TTF/DejaVuSans.ttf"),
    Path("/usr/share/fonts/dejavu/DejaVuSans.ttf"),
    Path("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
)
_SERIF_ROOTS = (
    Path("/usr/share/fonts/TTF/DejaVuSerif.ttf"),
    Path("/usr/share/fonts/dejavu/DejaVuSerif.ttf"),
)
_MONO_ROOTS = (
    Path("/usr/share/fonts/TTF/DejaVuSansMono.ttf"),
    Path("/usr/share/fonts/dejavu/DejaVuSansMono.ttf"),
)
_LIBERATION_ROOTS = (
    Path("/usr/share/fonts/liberation/LiberationSans-Regular.ttf"),
    Path("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf"),
)


def _system_font(candidates: tuple[Path, ...], label: str) -> Path:
    for candidate in candidates:
        if candidate.exists():
            return candidate
    pytest.skip(f"{label} system font not found; install DejaVu/Liberation fonts")


def _sans() -> Path:
    return _system_font(_FONT_ROOTS, "DejaVu Sans")


def _serif() -> Path:
    return _system_font(_SERIF_ROOTS, "DejaVu Serif")


def _mono() -> Path:
    return _system_font(_MONO_ROOTS, "DejaVu Sans Mono")


def _liberation() -> Path:
    return _system_font(_LIBERATION_ROOTS, "Liberation Sans")


def _schema() -> dict:
    return json.loads((_SCHEMAS / "font-v1.schema.json").read_text(encoding="utf-8"))


def _validate_report(report: ft.FontTranslationReport) -> None:
    jsonschema = pytest.importorskip("jsonschema")
    jsonschema.validate(
        instance=report.to_dict(),
        schema=_schema(),
        cls=jsonschema.Draft202012Validator,
    )


def _synthetic(
    family: str,
    classification: str,
    *,
    license: str | None = "OFL-1.1",
    **overrides: object,
) -> ft.FontMetadata:
    digits = family.lower().encode("utf-8").hex()[:64].ljust(64, "0")
    values: dict[str, object] = {
        "id": f"font-{family.lower()}",
        "family": family,
        "style": "Regular",
        "version": "1.000",
        "format": "ttf",
        "sha256": digits,
        "size_bytes": 1_000,
        "source": "web",
        "license": license,
        "embedding_allowed": True,
        "subset_allowed": True,
        "units_per_em": 1_000,
        "ascent": 800,
        "descent": -200,
        "x_height": 500,
        "cap_height": 700,
        "average_advance": 500.0,
        "glyph_count": 500,
        "classification": classification,
        "weight": 400,
        "width": 5,
        "width_name": "normal",
        "slant": "upright",
        "scripts": ["latin"],
        "features": ["tabular_nums"],
    }
    values.update(overrides)
    return ft.FontMetadata(**values)  # type: ignore[arg-type]


# ---------------------------------------------------------------------------
# Metadata extraction
# ---------------------------------------------------------------------------


def test_metadata_captures_names_metrics_panose_coverage_and_scripts():
    metadata = ft.FontMetadata.from_path(_sans())

    assert metadata.family.startswith("DejaVu Sans")
    assert metadata.style
    assert metadata.format == "ttf"
    assert len(metadata.sha256) == ft.SHA256_HEX_LEN
    assert metadata.id == f"font-{metadata.sha256[:16]}"
    assert metadata.private is True
    assert metadata.size_bytes == _sans().stat().st_size
    assert metadata.units_per_em > 0
    assert metadata.ascent > 0
    assert metadata.descent < 0
    assert metadata.glyph_count > 100
    assert metadata.average_advance and metadata.average_advance > 0
    assert metadata.glyph_coverage
    assert any(value.startswith("U+00") for value in metadata.glyph_coverage)
    assert ord("A") in metadata.codepoint_set()
    assert "latin" in metadata.scripts
    assert "kerning" in metadata.features
    assert "name" in metadata.tables
    assert metadata.license and len(metadata.license) > 20
    assert metadata.panose is not None
    panose = metadata.panose_value()
    assert panose is not None and len(panose.values) == 10
    assert metadata.os2_family_class is not None
    assert metadata.embedding_allowed is True
    assert metadata.subset_allowed is True
    assert metadata.bitmap_only is False
    assert metadata.fs_type == 0
    assert metadata.to_dict()["sha256"] == metadata.sha256


def test_metadata_metric_ratios_and_missing_glyph_range_parsing():
    metadata = ft.FontMetadata.from_path(_liberation())
    assert metadata.x_height is not None
    assert metadata.cap_height is not None
    assert metadata.x_height_ratio() == pytest.approx(
        metadata.x_height / metadata.units_per_em
    )
    assert metadata.cap_height_ratio() == pytest.approx(
        metadata.cap_height / metadata.units_per_em
    )
    assert metadata.covers_script("latin") is True
    assert metadata.covers_script("cjk") is False
    codepoints = metadata.codepoint_set()
    assert ord("A") in codepoints
    assert ord("\u4e2d") not in codepoints


def test_full_prose_licence_is_rejected_but_explicit_licence_is_accepted():
    prose = ft.FontMetadata.from_path(_sans())
    assert prose.license is not None
    assert prose.license_allowed() is False
    assert prose.can_embed() is False

    explicit = ft.FontMetadata.from_path(_sans(), license="OFL-1.1")
    assert explicit.license_allowed() is True
    assert explicit.can_embed() is True


@pytest.mark.parametrize(
    ("fs_type", "embedding", "subsetting", "bitmap", "restricted", "preview"),
    [
        (0x0000, True, True, False, False, False),
        (0x0002, False, False, False, True, False),
        (0x0004, False, False, False, False, True),
        (0x0100, True, False, False, False, False),
        (0x0200, False, False, True, False, False),
    ],
)
def test_fs_type_bits_are_decoded_conservatively(
    tmp_path, fs_type, embedding, subsetting, bitmap, restricted, preview
):
    font_tools = pytest.importorskip("fontTools.ttLib")
    target = tmp_path / f"fs-{fs_type:04x}.ttf"
    font = font_tools.TTFont(_sans())
    font["OS/2"].fsType = fs_type
    font.save(target)
    font.close()

    metadata = ft.FontMetadata.from_path(target, license="OFL-1.1")
    permissions = metadata.permissions()
    assert metadata.fs_type == fs_type
    assert metadata.embedding_allowed is embedding
    assert metadata.subset_allowed is subsetting
    assert metadata.bitmap_only is bitmap
    assert permissions.restricted is restricted
    assert permissions.preview_print_only is preview
    assert metadata.can_embed() is embedding


def test_panose_distance_is_bounded_symmetric_and_family_aware():
    base = "2 11 5 3 2 2 3 2 2 4"
    heavier = "2 11 7 3 2 2 3 2 2 4"
    script = "3 2 5 3 2 2 3 2 2 4"

    assert ft.panose_distance(base, base) == 0.0
    assert 0.0 < ft.panose_distance(base, heavier) < 1.0
    assert ft.panose_distance(base, heavier) == ft.panose_distance(heavier, base)
    assert ft.panose_distance(base, script) == 1.0
    assert ft.panose_distance("not panose", base) is None


def test_tabular_figure_feature_is_detected_from_tnum(tmp_path):
    font_tools = pytest.importorskip("fontTools.ttLib")
    ot_tables = pytest.importorskip("fontTools.ttLib.tables.otTables")

    target = tmp_path / "tnum.ttf"
    font = font_tools.TTFont(_sans())
    table = font["GSUB"].table
    record = ot_tables.FeatureRecord()
    record.FeatureTag = "tnum"
    feature = ot_tables.Feature()
    feature.FeatureParams = None
    feature.LookupListIndex = []
    record.Feature = feature
    table.FeatureList.FeatureRecord.append(record)
    table.FeatureList.FeatureCount = len(table.FeatureList.FeatureRecord)
    font.save(target)
    font.close()

    metadata = ft.FontMetadata.from_path(target)
    assert "tabular_nums" in metadata.features
    assert metadata.supports_feature("tnum") is True
    assert metadata.supports_feature("tabular-nums") is True


def test_serif_and_mono_classification():
    serif = ft.FontMetadata.from_path(_serif())
    mono = ft.FontMetadata.from_path(_mono())

    assert serif.classification == "serif"
    assert serif.covers_script("latin") is True
    assert mono.classification == "mono"
    assert mono.is_monospaced is True


# ---------------------------------------------------------------------------
# Matching
# ---------------------------------------------------------------------------


def test_matching_order_is_deterministic_for_ties():
    intent = ft.FontIntent.new("", "")
    intent.scripts = ["latin"]
    candidates = [
        _synthetic("Zeta", "sans"),
        _synthetic("alpha", "sans"),
        _synthetic("Mu", "sans"),
    ]

    first = [match.font.family for match in ft.match_fonts(intent, candidates)]
    assert first == ["alpha", "Mu", "Zeta"]

    second = [
        match.font.family
        for match in ft.match_fonts(intent, list(reversed(candidates)))
    ]
    assert second == first


def test_metric_compatibility_prefers_close_metrics():
    intent = ft.FontIntent.new("body", "sans")
    intent.traits.units_per_em = 1_000
    intent.traits.x_height = 500.0
    intent.traits.cap_height = 700.0
    intent.traits.ascent = 800
    intent.traits.descent = -200
    intent.traits.average_advance = 500.0

    close = _synthetic("Close", "sans")
    far = _synthetic(
        "Far",
        "sans",
        x_height=650,
        cap_height=550,
        ascent=950,
        descent=-50,
        average_advance=700.0,
    )

    close_score = ft.score_candidate(intent, close)
    far_score = ft.score_candidate(intent, far)
    assert close_score is not None and far_score is not None
    assert close_score.metrics > far_score.metrics
    assert close_score.total > far_score.total


def test_script_and_feature_coverage_are_scored_and_reported():
    intent = ft.FontIntent.new("numeric", "sans")
    intent.scripts = ["latin", "arabic"]
    intent.features = ["tnum", "zero"]

    latin_only = _synthetic("Latin Only", "sans")
    broad = _synthetic(
        "Broad",
        "sans",
        scripts=["latin", "arabic"],
        features=["tabular_nums", "slashed_zero"],
    )

    latin_score = ft.score_candidate(intent, latin_only)
    broad_score = ft.score_candidate(intent, broad)
    assert latin_score is not None and broad_score is not None
    assert latin_score.coverage < 1.0
    assert broad_score.coverage == 1.0
    assert broad_score.features > latin_score.features

    matches = ft.match_fonts(intent, [latin_only, broad])
    assert matches[0].font.family == "Broad"
    assert "arabic" in matches[1].missing_scripts
    assert "slashed_zero" in matches[1].missing_features


def test_classification_and_role_are_hard_filters():
    code_intent = ft.FontIntent.new("code", "mono")
    sans = _synthetic("Sans", "sans")
    mono = _synthetic("Mono", "mono")
    matches = ft.match_fonts(code_intent, [sans, mono])
    assert [match.font.family for match in matches] == ["Mono"]

    icon_intent = ft.FontIntent.new("icon", "icon")
    assert ft.choose_font(icon_intent, [mono]) is None

    body_intent = ft.FontIntent.new("body", "sans")
    serif = _synthetic("Serif", "serif")
    assert ft.choose_font(body_intent, [serif]) is None


def test_target_format_compatibility_affects_score():
    intent = ft.FontIntent.new("body", "sans")
    intent.target = ft.TargetFontProfile(target="browser", formats=["ttf"])
    ttf = _synthetic("TTF Sans", "sans")
    otf = _synthetic("OTF Sans", "sans", format="otf")

    ttf_score = ft.score_candidate(intent, ttf)
    otf_score = ft.score_candidate(intent, otf)
    assert ttf_score is not None and otf_score is not None
    assert ttf_score.format == 1.0
    assert otf_score.format == 0.0
    assert ttf_score.total > otf_score.total
    assert ft.candidate_supports_target(intent.target, ttf) is True


# ---------------------------------------------------------------------------
# Export planning
# ---------------------------------------------------------------------------


def test_plan_export_uses_original_exactly_and_report_is_schema_valid():
    original = _synthetic("Source Sans", "sans")
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(target="browser", formats=["ttf", "woff2"])

    report = ft.plan_export(intent, original, [], target)
    assert report.action == "exact"
    assert report.source_font == "Source Sans"
    assert report.target_font == "Source Sans"
    assert report.license_ok is True
    assert report.shape_score is None
    assert report.metrics is not None
    assert report.dropped_glyphs == []
    _validate_report(report)


def test_plan_export_subsets_oversized_original():
    original = _synthetic("Source Sans", "sans")
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(
        target="lvgl", formats=["ttf"], max_bytes=500
    )

    report = ft.plan_export(intent, original, [], target)
    assert report.action == "subset"
    assert report.target_font == "Source Sans"
    assert report.license_ok is True


def test_plan_export_substitutes_non_embeddable_original():
    original = _synthetic(
        "Restricted Sans",
        "sans",
        embedding_allowed=False,
        subset_allowed=False,
        fs_type=0x0002,
    )
    candidate = _synthetic("Open Sans", "sans")
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(target="browser", formats=["ttf"])

    report = ft.plan_export(intent, original, [candidate], target)
    assert report.action == "substituted"
    assert report.source_font == "Restricted Sans"
    assert report.target_font == "Open Sans"
    assert report.shape_score is not None
    assert report.license_ok is True
    assert any("does not permit embedding" in warning for warning in report.warnings)
    assert any("substituted" in warning for warning in report.warnings)
    _validate_report(report)


def test_plan_export_rasterizes_for_bitmap_targets():
    original = _synthetic("Source Sans", "sans")
    intent = ft.FontIntent.new("label", "sans")
    target = ft.TargetFontProfile(target="eink", formats=["bdf"], bpp=1)

    report = ft.plan_export(intent, original, [], target)
    assert report.action == "rasterized"
    assert report.target_font == "Source Sans"
    assert report.license_ok is True


def test_plan_export_reports_unavailable_and_dropped_coverage():
    original = _synthetic("Source Sans", "sans", license="GPL-2.0-only")
    intent = ft.FontIntent.new("body", "sans")
    intent.scripts = ["latin"]
    intent.features = ["tnum"]
    target = ft.TargetFontProfile(target="web", formats=["woff2"])

    report = ft.plan_export(intent, original, [], target)
    assert report.action == "unavailable"
    assert report.target_font == ""
    assert report.license_ok is False
    assert report.dropped_glyphs == ["script:latin"]
    assert report.dropped_features == ["tabular_nums"]
    assert any("licence" in warning for warning in report.warnings)
    # The in-memory Rust shape is preserved; to_dict substitutes a placeholder
    # so the JSON report still satisfies the schema's minLength constraint.
    assert report.to_dict()["target_font"] == "unavailable"
    _validate_report(report)


def test_plan_export_can_substitute_for_rejected_licence():
    original = _synthetic("GPL Sans", "sans", license="GPL-2.0-only")
    candidate = _synthetic("OFL Sans", "sans")
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(target="browser", formats=["ttf"])

    report = ft.plan_export(intent, original, [candidate], target)
    assert report.action == "substituted"
    assert report.target_font == "OFL Sans"
    assert report.license_ok is True
    _validate_report(report)


def test_intent_typography_and_target_round_trip():
    intent = ft.FontIntent.from_dict(
        {
            "id": "body",
            "role": "body",
            "classification": "sans",
            "traits": {"weight": 500, "slant": "upright"},
            "scripts": ["latin", "latin_ext"],
            "features": ["tnum"],
            "target": {"target": "browser", "formats": ["ttf", "woff2"]},
            "fallbacks": [{"family": "DejaVu Sans"}],
        }
    )
    assert intent.traits.weight == 500
    assert intent.target is not None and intent.target.supports_format("truetype")
    assert intent.to_dict()["target"]["formats"] == ["ttf", "woff2"]
    assert intent.to_dict()["fallbacks"] == ["DejaVu Sans"]

    jsonschema = pytest.importorskip("jsonschema")
    schema = _schema()
    jsonschema.validate(intent.to_dict(), schema, cls=jsonschema.Draft202012Validator)
    jsonschema.validate(
        intent.target.to_dict(), schema, cls=jsonschema.Draft202012Validator
    )

    typography = ft.TypographyRef.from_dict(
        {"intent": "body", "size_px": 14, "line_height": 1.4, "features": ["tnum"]}
    )
    assert typography.size_px == 14
    assert typography.to_dict()["features"] == ["tnum"]
