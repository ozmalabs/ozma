# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Golden-corpus and shaping/coverage tests for the font subsystem (WGT-056).

The corpus itself lives in ``tests/fixtures/fonts/golden.json``: metadata
expectations and ranking scores only, keyed by font basename. No font binaries
are stored. System fonts are resolved through fontconfig (``fc-list``), with a
plain filesystem walk as a fallback, so the suite works on hosts with different
font package layouts. Fonts or scripts that are not installed skip with an
explicit reason instead of failing.
"""

from __future__ import annotations

import dataclasses
import functools
import hashlib
import json
import random
import shutil
import subprocess
from pathlib import Path
from typing import Any

import pytest

import font_export as fe
import font_translation as ft

pytestmark = pytest.mark.unit

_FIXTURE = (
    Path(__file__).resolve().parents[2] / "fixtures" / "fonts" / "golden.json"
)
_SCHEMAS = Path(__file__).resolve().parents[3] / "schemas" / "display"
_FONT_DIRS = (
    Path("/usr/share/fonts"),
    Path("/usr/local/share/fonts"),
    Path.home() / ".local" / "share" / "fonts",
    Path.home() / ".fonts",
)
_FONT_SUFFIXES = {".ttf", ".otf", ".ttc", ".otc"}


# ---------------------------------------------------------------------------
# Discovery
# ---------------------------------------------------------------------------


@functools.lru_cache(maxsize=1)
def _font_records() -> tuple[tuple[Path, tuple[str, ...], int], ...]:
    """Return ``(path, families, face_index)`` records for installed fonts.

    ``fc-list`` is authoritative when present because it exposes collection
    face indices. The filesystem walk is only a fallback for minimal images.
    """

    records: list[tuple[Path, tuple[str, ...], int]] = []
    fc_list = shutil.which("fc-list")
    if fc_list:
        try:
            result = subprocess.run(
                [
                    fc_list,
                    ":",
                    "--format",
                    "%{file}\t%{family}\t%{index}\n",
                ],
                capture_output=True,
                text=True,
                check=False,
                timeout=30,
            )
        except (OSError, subprocess.SubprocessError):
            result = None
        if result is not None and result.returncode == 0:
            for line in result.stdout.splitlines():
                parts = line.split("\t")
                if len(parts) < 2 or not parts[0]:
                    continue
                path = Path(parts[0])
                families = tuple(
                    value.strip()
                    for value in parts[1].split(",")
                    if value.strip()
                )
                try:
                    face_index = int(parts[2]) if len(parts) > 2 else 0
                except ValueError:
                    face_index = 0
                records.append((path, families, face_index))
    if not records:
        for root in _FONT_DIRS:
            if not root.is_dir():
                continue
            for path in sorted(root.rglob("*")):
                if path.suffix.lower() in _FONT_SUFFIXES:
                    records.append((path, (), 0))
    return tuple(records)


def _resolve_font(entry: dict[str, Any]) -> Path | None:
    basename = str(entry["basename"]).casefold()
    expected_family = str(entry.get("family") or "")
    face_index = int(entry.get("face_index", 0))
    matches: list[Path] = []
    for path, families, index in _font_records():
        if path.name.casefold() != basename:
            continue
        if expected_family and families and expected_family not in families:
            continue
        if index != face_index:
            continue
        matches.append(path)
    if matches:
        return sorted(set(matches))[0]
    return None


@functools.lru_cache(maxsize=1)
def _golden() -> dict[str, Any]:
    return json.loads(_FIXTURE.read_text(encoding="utf-8"))


def _font_keys() -> list[str]:
    return list(_golden()["fonts"])


def _font_entry(key: str) -> dict[str, Any]:
    return _golden()["fonts"][key]


def _require_font(key: str) -> tuple[Path, dict[str, Any]]:
    entry = _font_entry(key)
    if entry.get("expected") is None:
        pytest.skip(f"{key}: not installed ({entry.get('skip_reason', 'absent')})")
    path = _resolve_font(entry)
    if path is None:
        pytest.skip(
            f"{key}: {entry['basename']} not installed "
            f"({entry.get('skip_reason', 'absent')})"
        )
    return path, entry


@functools.lru_cache(maxsize=None)
def _metadata(
    path: str, face_index: int, license: str | None
) -> ft.FontMetadata:
    return ft.FontMetadata.from_path(
        path, face_index=face_index, license=license
    )


def _font_metadata(key: str, *, license: str | None = None) -> ft.FontMetadata:
    path, entry = _require_font(key)
    return _metadata(str(path), int(entry.get("face_index", 0)), license)


def _schema() -> dict[str, Any]:
    return json.loads((_SCHEMAS / "font-v1.schema.json").read_text(encoding="utf-8"))


def _validate_report(report: ft.FontTranslationReport) -> None:
    jsonschema = pytest.importorskip("jsonschema")
    jsonschema.validate(
        instance=report.to_dict(),
        schema=_schema(),
        cls=jsonschema.Draft202012Validator,
    )


def _ttfont():
    return pytest.importorskip("fontTools.ttLib").TTFont


# ---------------------------------------------------------------------------
# Golden metadata
# ---------------------------------------------------------------------------


def _stable_dict(metadata: ft.FontMetadata) -> dict[str, Any]:
    data = metadata.to_dict()
    data.pop("path", None)
    return data


def _assert_metadata_matches(
    metadata: ft.FontMetadata,
    expected: dict[str, Any],
    tolerance: float,
) -> None:
    for field, value in expected.items():
        if field == "average_advance":
            if value is None:
                assert metadata.average_advance is None
            else:
                assert metadata.average_advance == pytest.approx(value, abs=tolerance)
        elif field == "variable_axes":
            assert metadata.variable_axes.keys() == value.keys()
            for tag, axis_value in value.items():
                assert metadata.variable_axes[tag] == pytest.approx(
                    axis_value, abs=tolerance
                )
        elif field == "variable_axis_ranges":
            actual = [axis.to_dict() for axis in metadata.variable_axis_ranges]
            assert len(actual) == len(value)
            for got, want in zip(actual, value):
                assert got["tag"] == want["tag"]
                assert got["name"] == want["name"]
                assert got["hidden"] == want["hidden"]
                for dimension in ("min", "default", "max"):
                    assert got[dimension] == pytest.approx(
                        want[dimension], abs=tolerance
                    )
        elif field == "license_allowed":
            assert metadata.license_allowed() is value
        else:
            assert getattr(metadata, field) == value


@pytest.mark.parametrize("key", _font_keys())
def test_golden_metadata_is_deterministic(key):
    path, entry = _require_font(key)
    expected = entry["expected"]
    tolerance = float(_golden()["metric_tolerance"])
    face_index = int(entry.get("face_index", 0))

    first = ft.FontMetadata.from_path(path, face_index=face_index)
    second = ft.FontMetadata.from_path(path, face_index=face_index)
    assert _stable_dict(first) == _stable_dict(second)

    raw = path.read_bytes()
    assert first.sha256 == hashlib.sha256(raw).hexdigest()
    assert len(first.sha256) == ft.SHA256_HEX_LEN
    assert first.id == f"font-{first.sha256[:16]}"
    assert first.size_bytes == len(raw)
    assert first.path == str(path)

    # In-memory parsing must produce the same metadata as path parsing; only
    # the local ``path`` field may differ.
    if first.size_bytes <= 8_000_000:
        from_bytes = ft.FontMetadata.from_bytes(
            raw, face_index=face_index
        )
        assert _stable_dict(from_bytes) == _stable_dict(first)

    _assert_metadata_matches(first, expected, tolerance)
    assert first.codepoint_set()
    for script in expected["scripts"]:
        assert first.covers_script(script)
    for feature in expected["features"]:
        assert first.supports_feature(feature)


# ---------------------------------------------------------------------------
# Deterministic ranking
# ---------------------------------------------------------------------------


def test_shuffled_candidate_order_matches_golden_ranking():
    matching = _golden()["matching"]
    intent = ft.FontIntent.from_dict(matching["intent"])
    candidates: list[ft.FontMetadata] = []
    families: dict[str, str] = {}
    for key in matching["candidates"]:
        path, entry = _require_font(key)
        candidates.append(
            _metadata(str(path), int(entry.get("face_index", 0)), "OFL-1.1")
        )
        families[key] = entry["expected"]["family"]

    expected_order = [families[key] for key in matching["expected_order"]]
    expected_scores = {
        families[key]: score
        for key, score in matching["expected_scores"].items()
    }
    tolerance = float(_golden()["score_tolerance"])

    def ranked(
        corpus: list[ft.FontMetadata],
    ) -> list[tuple[str, float, list[str], list[str]]]:
        return [
            (
                match.font.family,
                match.score,
                match.missing_features,
                match.missing_scripts,
            )
            for match in ft.match_fonts(intent, corpus)
        ]

    baseline = ranked(candidates)
    assert [row[0] for row in baseline] == expected_order
    for family, score, _features, _scripts in baseline:
        assert score == pytest.approx(expected_scores[family], abs=tolerance)

    for family, _score, features, scripts in baseline:
        key_by_family = {value: key for key, value in families.items()}
        key = key_by_family[family]
        assert features == matching["expected_missing_features"].get(key, [])
        assert scripts == matching["expected_missing_scripts"].get(key, [])

    rng = random.Random(2056)
    for _ in range(5):
        shuffled = list(candidates)
        rng.shuffle(shuffled)
        assert ranked(shuffled) == baseline
    assert ranked(list(reversed(candidates))) == baseline


# ---------------------------------------------------------------------------
# Latin, CJK, Arabic, Indic coverage
# ---------------------------------------------------------------------------


def test_latin_corpus_covers_sans_serif_and_mono():
    roles = {
        "dejavu_sans_regular": "sans",
        "dejavu_serif_regular": "serif",
        "dejavu_sans_mono_regular": "mono",
    }
    seen: set[str] = set()
    for key, expected_classification in roles.items():
        metadata = _font_metadata(key)
        assert metadata.covers_script("latin") is True
        assert ord("A") in metadata.codepoint_set()
        assert metadata.classification == expected_classification
        assert metadata.units_per_em > 0
        assert metadata.ascent > 0
        assert metadata.descent < 0
        assert metadata.style
        seen.add(metadata.classification)
    assert {"sans", "serif", "mono"} <= seen


@pytest.mark.parametrize(
    ("key", "scripts", "codepoints", "features"),
    [
        (
            "noto_sans_cjk_jp_regular",
            ("cjk", "hiragana", "katakana", "hangul"),
            (0x4E2D, 0x3042, 0x30A2, 0xAC00),
            ("locl",),
        ),
        (
            "source_han_sans_cn_light",
            ("cjk", "hiragana", "katakana"),
            (0x4E2D, 0x3042, 0x30A2),
            ("locl",),
        ),
        (
            "noto_sans_arabic_regular",
            ("arabic",),
            (0x0627, 0x0644, 0x0645),
            ("init", "medi", "fina", "rlig"),
        ),
        (
            "noto_naskh_arabic_regular",
            ("arabic",),
            (0x0627, 0x0644, 0x0645),
            ("init", "medi", "fina", "rlig"),
        ),
        (
            "noto_sans_devanagari_regular",
            ("devanagari",),
            (0x0915, 0x093F, 0x094D),
            ("rphf", "half", "akhn"),
        ),
    ],
)
def test_script_coverage_and_shaping_features(key, scripts, codepoints, features):
    metadata = _font_metadata(key)
    codepoint_set = metadata.codepoint_set()

    for script in scripts:
        assert metadata.covers_script(script) is True, script
    for codepoint in codepoints:
        assert codepoint in codepoint_set, hex(codepoint)
    for feature in features:
        assert metadata.supports_feature(feature) is True, feature
    assert metadata.glyph_coverage

    intent = ft.FontIntent.new("body", "sans")
    intent.scripts = list(scripts)
    intent.features = list(features)
    score = ft.score_candidate(intent, metadata)
    assert score is not None
    assert score.coverage == 1.0
    assert score.features == 1.0
    assert ft.missing_scripts(intent, metadata) == []
    assert ft.missing_features(intent, metadata) == []


def test_wenquanyi_cjk_alternative_or_explicit_skip():
    entry = _font_entry("wenquanyi_zenhei")
    path = _resolve_font(entry)
    if path is None:
        pytest.skip(
            "WenQuanYi Zen Hei not installed; CJK coverage uses Noto/Source Han "
            f"({entry.get('skip_reason', 'absent')})"
        )
    metadata = _metadata(str(path), int(entry.get("face_index", 0)), None)
    assert metadata.covers_script("cjk") is True
    assert 0x4E2D in metadata.codepoint_set()


# ---------------------------------------------------------------------------
# Icons, colour fonts, variable axes, tabular figures
# ---------------------------------------------------------------------------


def _is_icon_or_colour(metadata: ft.FontMetadata) -> bool:
    return (
        metadata.classification == "icon"
        or metadata.has_color_glyphs
        or "emoji" in metadata.scripts
    )


def test_icon_and_colour_fonts_do_not_substitute_silently():
    target = ft.TargetFontProfile(target="browser", formats=["ttf", "otf"])
    icon = _font_metadata("font_awesome_7_solid", license="OFL-1.1")
    emoji = _font_metadata("noto_color_emoji", license="OFL-1.1")
    sans = _font_metadata("dejavu_sans_regular", license="OFL-1.1")

    assert _is_icon_or_colour(icon) is True
    assert icon.classification == "icon"
    assert _is_icon_or_colour(emoji) is True
    assert emoji.has_color_glyphs is True
    assert "emoji" in emoji.scripts

    icon_intent = ft.FontIntent.new("icon", "icon")
    icon_intent.scripts = ["symbols"]
    assert [match.font.family for match in ft.match_fonts(icon_intent, [sans])] == []
    assert ft.plan_export(icon_intent, icon, [], target).action == "exact"

    report = ft.plan_export(icon_intent, None, [sans], target)
    assert report.action == "unavailable"
    assert report.to_dict()["target_font"] == "unavailable"
    assert "DejaVu Sans" not in report.to_dict()["target_font"]

    # A non-embeddable icon source must not fall back to an unrelated sans
    # face. Colour-only metadata is retained on the report path.
    restricted = dataclasses.replace(
        emoji,
        embedding_allowed=False,
        subset_allowed=False,
        license="proprietary",
        fs_type=0x0002,
    )
    report = ft.plan_export(icon_intent, restricted, [sans], target)
    assert report.action == "unavailable"
    assert report.target_font == ""
    assert any(
        "does not permit embedding" in warning for warning in report.warnings
    )
    assert any("licence" in warning for warning in report.warnings)
    _validate_report(report)


def test_variable_axes_and_tabular_figures_are_detected():
    metadata = _font_metadata("inter_variable", license="OFL-1.1")
    assert metadata.is_variable is True
    axes = {axis.tag: axis for axis in metadata.variable_axis_ranges}
    assert {"opsz", "wght"} <= set(axes)
    assert axes["wght"].min == pytest.approx(100.0)
    assert axes["wght"].default == pytest.approx(400.0)
    assert axes["wght"].max == pytest.approx(900.0)
    assert axes["wght"].name
    assert metadata.variable_axes["wght"] == pytest.approx(400.0)
    assert metadata.supports_feature("tnum") is True
    assert ft.canonical_feature_name("tnum") == "tabular_nums"

    intent = ft.FontIntent.new("numeric", "sans")
    intent.scripts = ["latin"]
    intent.features = ["tnum"]
    target = ft.TargetFontProfile(target="browser", formats=["ttf"])
    report = ft.plan_export(intent, metadata, [], target)
    assert report.action == "exact"
    assert report.dropped_features == []
    _validate_report(report)


def test_tabular_figure_feature_is_reported_for_noto_sans():
    noto = _font_metadata("noto_sans_regular")
    assert noto.supports_feature("tnum") is True
    assert noto.supports_feature("tabular-nums") is True
    assert "tabular_nums" in noto.features

    dejavu = _font_metadata("dejavu_sans_regular")
    assert dejavu.supports_feature("tnum") is False

    intent = ft.FontIntent.new("numeric", "sans")
    intent.scripts = ["latin"]
    intent.features = ["tnum"]
    covered = ft.score_candidate(intent, noto)
    uncovered = ft.score_candidate(intent, dejavu)
    assert covered is not None and uncovered is not None
    assert covered.features == 1.0
    assert uncovered.features == 0.0


# ---------------------------------------------------------------------------
# Export planning and physical export
# ---------------------------------------------------------------------------


def _body_intent() -> ft.FontIntent:
    intent = ft.FontIntent.new("body", "sans")
    intent.scripts = ["latin"]
    intent.features = ["tnum"]
    return intent


def test_plan_export_exact_and_subset_are_explicit(tmp_path):
    metadata = _font_metadata("liberation_sans_regular", license="OFL-1.1")
    intent = _body_intent()

    exact_target = ft.TargetFontProfile(target="browser", formats=["ttf"])
    exact = ft.plan_export(intent, metadata, [], exact_target)
    assert exact.action == "exact"
    assert exact.target_font == metadata.family
    assert exact.license_ok is True
    assert exact.dropped_glyphs == []
    assert exact.dropped_features == ["tabular_nums"]
    _validate_report(exact)

    subset_target = ft.TargetFontProfile(
        target="browser", formats=["ttf"], max_bytes=1000
    )
    subset = ft.plan_export(intent, metadata, [], subset_target)
    assert subset.action == "subset"
    assert subset.target_font == metadata.family
    assert subset.license_ok is True
    assert subset.dropped_features == ["tabular_nums"]
    _validate_report(subset)

    output = tmp_path / "subset.ttf"
    result = fe.plan_and_export(
        intent,
        metadata,
        [],
        subset_target,
        output,
        glyphs=["A", "B", "U+0043-U+0044"],
    )
    assert result["action"] == "subset"
    assert result["exported"] is True
    assert result["format"] == "ttf"
    assert result["dropped_features"] == ["tabular_nums"]
    assert output.exists() and output.stat().st_size < metadata.size_bytes

    font = _ttfont()(output)
    try:
        cmap = font.getBestCmap()
        assert {0x41, 0x42, 0x43, 0x44}.issubset(cmap)
        assert 0x5A not in cmap
        for table in ("name", "OS/2", "head", "hhea", "maxp", "post", "cmap"):
            assert table in font
    finally:
        font.close()


def _restricted_copy(tmp_path: Path, source: Path, fs_type: int = 0x0002) -> Path:
    target = tmp_path / f"restricted-{fs_type:04x}.ttf"
    font = _ttfont()(source)
    try:
        font["OS/2"].fsType = fs_type
        font.save(target)
    finally:
        font.close()
    return target


def test_plan_export_substituted_records_drops_and_licence(tmp_path):
    source_path, _entry = _require_font("liberation_sans_regular")
    restricted = ft.FontMetadata.from_path(
        _restricted_copy(tmp_path, source_path), license="OFL-1.1"
    )
    candidate = _font_metadata("dejavu_sans_regular", license="OFL-1.1")
    intent = _body_intent()
    target = ft.TargetFontProfile(target="browser", formats=["ttf"])

    report = ft.plan_export(intent, restricted, [candidate], target)
    assert report.action == "substituted"
    assert report.source_font == restricted.family
    assert report.target_font == candidate.family
    assert report.shape_score is not None
    assert report.license_ok is True
    assert report.dropped_glyphs == []
    assert report.dropped_features == ["tabular_nums"]
    assert any(
        "does not permit embedding" in warning for warning in report.warnings
    )
    assert any("substituted" in warning for warning in report.warnings)
    _validate_report(report)


def test_plan_export_unavailable_is_explicit():
    original = _synthetic_metadata(
        "GPL Sans",
        license="GPL-2.0-only",
        scripts=["latin"],
        features=["tabular_nums"],
    )
    intent = _body_intent()
    target = ft.TargetFontProfile(target="web", formats=["woff2"])

    report = ft.plan_export(intent, original, [], target)
    assert report.action == "unavailable"
    assert report.target_font == ""
    assert report.license_ok is False
    assert report.dropped_glyphs == ["script:latin"]
    assert report.dropped_features == ["tabular_nums"]
    assert any("licence" in warning for warning in report.warnings)
    assert report.to_dict()["target_font"] == "unavailable"
    _validate_report(report)


def test_subset_over_cjk_script_range_preserves_tables(tmp_path):
    metadata = _font_metadata("source_han_sans_cn_light", license="OFL-1.1")
    intent = ft.FontIntent.new("body", "sans")
    intent.scripts = ["latin", "cjk"]
    intent.features = ["locl"]
    target = ft.TargetFontProfile(
        target="browser", formats=["otf"], max_bytes=1000
    )

    plan = ft.plan_export(intent, metadata, [], target)
    assert plan.action == "subset"
    assert plan.dropped_glyphs == []
    assert plan.dropped_features == []
    _validate_report(plan)

    output = tmp_path / "cjk-subset.otf"
    result = fe.plan_and_export(
        intent,
        metadata,
        [],
        target,
        output,
        glyphs=["A", "U+4E00-U+4E05", "中", "U+10FFFF"],
    )
    assert result["action"] == "subset"
    assert result["exported"] is True
    assert result["format"] == "otf"
    assert result["dropped_glyphs"] == ["U+10FFFF"]
    assert output.read_bytes().startswith(b"OTTO")
    assert output.stat().st_size < metadata.size_bytes

    font = _ttfont()(output)
    try:
        cmap = font.getBestCmap()
        assert {0x41, 0x4E00, 0x4E05, 0x4E2D}.issubset(cmap)
        assert 0x9FFF not in cmap
        for table in (
            "name",
            "OS/2",
            "head",
            "hhea",
            "maxp",
            "post",
            "cmap",
            "GSUB",
            "GPOS",
        ):
            assert table in font
        assert font["OS/2"].fsType == metadata.fs_type
        licence_names = [
            record.toUnicode()
            for record in font["name"].names
            if record.nameID == 13
        ]
        assert any(value.strip() for value in licence_names)
    finally:
        font.close()

    # Missing glyph reporting is explicit for bitmap exports.
    missing = fe.export_bdf(
        metadata,
        ["A", "U+10FFFF"],
        tmp_path / "missing.bdf",
        pixel_size=16,
    )
    assert missing["missing_glyphs"] == ["U+10FFFF"]


def test_woff2_path_fails_closed_or_succeeds_with_brotli(tmp_path):
    woff2 = pytest.importorskip("fontTools.ttLib.woff2")
    metadata = _font_metadata("liberation_sans_regular", license="OFL-1.1")
    output = tmp_path / "subset.woff2"

    if not woff2.haveBrotli:
        with pytest.raises(fe.FontExportError, match="brotli") as caught:
            fe.subset_font(metadata, ["A", "B"], output, flavour="woff2")
        assert isinstance(caught.value, fe.FontExportError)
        assert not isinstance(caught.value, fe.FontSubsetError)
        assert not output.exists()
        return

    fe.subset_font(metadata, ["A", "B"], output, flavour="woff2")
    assert output.read_bytes().startswith(b"wOF2")
    assert output.stat().st_size > 0


def test_bdf_and_lvgl_handle_non_ascii_names(tmp_path):
    base = _font_metadata("source_han_sans_cn_light", license="OFL-1.1")
    metadata = dataclasses.replace(
        base,
        family="Source Han 中文",
        style="常规",
        copyright="© 2026 著作者",
    )

    bdf_path = tmp_path / "non-ascii.bdf"
    bdf = fe.export_bdf(metadata, ["A", "中"], bdf_path, pixel_size=16)
    assert bdf["glyphs"] == 2
    assert bdf["missing_glyphs"] == []
    bdf_text = bdf_path.read_text(encoding="utf-8")
    font_line = next(
        line for line in bdf_text.splitlines() if line.startswith("FONT ")
    )
    assert font_line.isascii()
    assert 'FAMILY_NAME "Source Han 中文"' in bdf_text
    assert 'COPYRIGHT "© 2026 著作者"' in bdf_text
    assert "ENCODING 20013" in bdf_text
    assert bdf_text.startswith("STARTFONT 2.1")

    c_path = tmp_path / "non-ascii.c"
    lvgl = fe.export_lvgl_c(
        metadata, ["A", "中"], c_path, bpp=4, pixel_size=16
    )
    assert lvgl["glyphs"] == 2
    assert lvgl["missing_glyphs"] == []
    c_text = c_path.read_text(encoding="utf-8")
    assert "/* Family: Source Han 中文 */" in c_text
    assert "/* Style: 常规 */" in c_text
    assert "/* Copyright: © 2026 著作者 */" in c_text
    assert "font_source_han" in c_text

    sidecar = json.loads(Path(lvgl["sidecar"]).read_text(encoding="utf-8"))
    assert sidecar["family"] == "Source Han 中文"
    assert sidecar["style"] == "常规"
    assert [glyph["codepoint"] for glyph in sidecar["glyphs"]] == [65, 20013]


# ---------------------------------------------------------------------------
# Synthetic coverage for scripts/features that system fonts may not provide
# ---------------------------------------------------------------------------


def _synthetic_metadata(
    family: str,
    *,
    classification: str = "sans",
    license: str | None = "OFL-1.1",
    **overrides: Any,
) -> ft.FontMetadata:
    digest = hashlib.sha256(family.encode("utf-8")).hexdigest()
    values: dict[str, Any] = {
        "id": f"font-{digest[:16]}",
        "family": family,
        "style": "Regular",
        "format": "ttf",
        "sha256": digest,
        "size_bytes": 1000,
        "source": "generated",
        "license": license,
        "embedding_allowed": True,
        "subset_allowed": True,
        "units_per_em": 1000,
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
    return ft.FontMetadata(**values)


def _build_synthetic_font(
    path: Path,
    codepoints: list[int],
    *,
    features: tuple[str, ...] = (),
) -> None:
    font_builder = pytest.importorskip("fontTools.fontBuilder")
    tt_glyph_pen = pytest.importorskip("fontTools.pens.ttGlyphPen")

    names = [".notdef"] + [f"uni{value:04X}" for value in codepoints]
    cmap = {value: f"uni{value:04X}" for value in codepoints}
    builder = font_builder.FontBuilder(1000, isTTF=True)
    builder.setupGlyphOrder(names)
    builder.setupCharacterMap(cmap)
    builder.setupGlyf(
        {name: tt_glyph_pen.TTGlyphPen(None).glyph() for name in names}
    )
    builder.setupHorizontalMetrics({name: (500, 0) for name in names})
    builder.setupHorizontalHeader(ascent=800, descent=-200)
    builder.setupNameTable(
        {"familyName": "Synthetic Coverage", "styleName": "Regular"}
    )
    builder.setupOS2(
        sTypoAscender=800,
        sTypoDescender=-200,
        usWinAscent=800,
        usWinDescent=200,
    )
    builder.setupPost()
    builder.setupMaxp()

    font = builder.font
    if features:
        feature_path = path.with_suffix(".fea")
        rules = ["languagesystem DFLT dflt;"]
        if "tnum" in features:
            rules.append("feature tnum { sub uni0030 by uni0031; } tnum;")
        if "smcp" in features:
            rules.append("feature smcp { sub uni0061 by uni0041; } smcp;")
        if "liga" in features:
            rules.append("feature liga { sub uni0066 uni0069 by uni0030; } liga;")
        feature_path.write_text("\n".join(rules) + "\n", encoding="utf-8")
        try:
            from fontTools.feaLib.builder import addOpenTypeFeatures

            addOpenTypeFeatures(font, str(feature_path))
        finally:
            feature_path.unlink()
    font.save(path)


def test_synthetic_cmap_ranges_and_features(tmp_path):
    codepoints = [
        0x0030,
        0x0031,
        0x0041,
        0x0061,
        0x0066,
        0x0069,
        0x0905,
        0x0B80,
        0x0E01,
        0x0E02,
        0x13A0,
        0x4E00,
        0xAC00,
        0xE000,
        0x1F600,
    ]
    path = tmp_path / "synthetic-coverage.ttf"
    _build_synthetic_font(path, codepoints, features=("tnum", "smcp", "liga"))
    metadata = ft.FontMetadata.from_path(path)

    assert metadata.scripts == [
        "cherokee",
        "cjk",
        "devanagari",
        "emoji",
        "hangul",
        "latin",
        "private-use",
        "symbols",
        "tamil",
        "thai",
    ]
    assert metadata.features == ["ligatures", "small_caps", "tabular_nums"]
    assert metadata.glyph_coverage == [
        "U+0030-U+0031",
        "U+0041",
        "U+0061",
        "U+0066",
        "U+0069",
        "U+0905",
        "U+0B80",
        "U+0E01-U+0E02",
        "U+13A0",
        "U+4E00",
        "U+AC00",
        "U+E000",
        "U+1F600",
    ]
    for feature in ("tnum", "smcp", "liga"):
        assert metadata.supports_feature(feature) is True

    # Classification helpers are also covered directly for scripts that no
    # installed system font may contain.
    assert ft._classify_scripts({0x0E01}) == ["thai"]
    assert ft._classify_scripts({0x13A0}) == ["cherokee"]
    assert ft._classify_scripts({0xE000}) == ["private-use"]
    assert ft._classify_scripts({0x1F600}) == ["emoji", "symbols"]

    intent = ft.FontIntent.new("body", "sans")
    intent.scripts = ["thai", "cherokee", "cjk", "emoji"]
    intent.features = ["tnum", "smcp", "liga"]
    score = ft.score_candidate(intent, metadata)
    assert score is not None
    assert score.coverage == 1.0
    assert score.features == 1.0

    latin_only = _synthetic_metadata("Latin Only")
    matches = ft.match_fonts(intent, [latin_only])
    assert matches[0].missing_scripts == [
        "cherokee",
        "cjk",
        "emoji",
        "thai",
    ]
    assert matches[0].missing_features == ["ligatures", "small_caps"]


def test_golden_fixture_is_path_independent_and_machine_readable():
    raw = _FIXTURE.read_text(encoding="utf-8")
    assert "/usr/" not in raw
    assert "/home/" not in raw
    data = json.loads(raw)
    assert data["schema_version"] == 1
    assert data["fonts"]
    assert data["matching"]["expected_order"]
    for key, entry in data["fonts"].items():
        assert key
        assert entry["basename"]
        assert not Path(entry["basename"]).is_absolute()
        assert "face_index" in entry
        assert "optional" in entry
        assert "skip_reason" in entry
        assert "expected" in entry
        if entry["expected"] is not None:
            expected = entry["expected"]
            assert len(expected["sha256"]) == ft.SHA256_HEX_LEN
            assert expected["family"]
            assert expected["scripts"]
            assert "fs_type" in expected
            assert "panose" in expected
