# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for physical font subset, bitmap, LVGL, and preview export."""

from __future__ import annotations

import ctypes.util
import importlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

import brotli_fallback
import font_export as fe
import font_translation as ft

pytestmark = pytest.mark.unit

_FONT_ROOTS = (
    Path("/usr/share/fonts/TTF/DejaVuSans.ttf"),
    Path("/usr/share/fonts/dejavu/DejaVuSans.ttf"),
    Path("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
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


def _liberation() -> Path:
    return _system_font(_LIBERATION_ROOTS, "Liberation Sans")


def _font_tools():
    return pytest.importorskip("fontTools.ttLib")


def _copy_with_fs_type(tmp_path: Path, fs_type: int, source: Path | None = None) -> Path:
    TTFont = _font_tools().TTFont
    target = tmp_path / f"restricted-{fs_type:04x}.ttf"
    font = TTFont(source or _sans())
    font["OS/2"].fsType = fs_type
    font.save(target)
    font.close()
    return target


@pytest.fixture
def woff2_state():
    """Restore Brotli module entries and fontTools' WOFF2 flag after a test."""

    from fontTools.ttLib import woff2

    saved = {
        name: sys.modules.get(name) for name in ("brotli", "brotlicffi")
    }
    for name, module in saved.items():
        if module is None or getattr(module, "_ozma_ctypes_fallback", False):
            sys.modules.pop(name, None)
    importlib.reload(woff2)
    try:
        yield woff2
    finally:
        for name in ("brotli", "brotlicffi"):
            sys.modules.pop(name, None)
        for name, module in saved.items():
            if module is not None:
                sys.modules[name] = module
        importlib.reload(woff2)


_LVGL_HEADER = """\
#ifndef LVGL_H
#define LVGL_H
#include <stdint.h>
#include <stdbool.h>

#define LV_ATTRIBUTE_LARGE_CONST
#define LV_FONT_SUBPX_NONE 0
#define LV_FONT_FMT_TXT_CMAP_SPARSE_TINY 0

typedef enum {
    LV_FONT_FMT_TXT_CMAP_FORMAT0_TINY = 0,
    LV_FONT_FMT_TXT_CMAP_SPARSE_TINY_VALUE = 1
} lv_font_fmt_txt_cmap_type_t;

typedef struct {
    uint32_t bitmap_index;
    uint16_t adv_w;
    uint16_t box_w;
    uint16_t box_h;
    int16_t ofs_x;
    int16_t ofs_y;
} lv_font_fmt_txt_glyph_dsc_t;

typedef struct {
    uint32_t range_start;
    uint16_t range_length;
    uint16_t glyph_id_start;
    const uint16_t * unicode_list;
    const void * glyph_id_ofs_list;
    uint16_t list_length;
    lv_font_fmt_txt_cmap_type_t type;
} lv_font_fmt_txt_cmap_t;

typedef struct {
    const uint8_t * glyph_bitmap;
    const lv_font_fmt_txt_glyph_dsc_t * glyph_dsc;
    const lv_font_fmt_txt_cmap_t * cmaps;
    const void * kern_dsc;
    uint16_t kern_scale;
    uint8_t cmap_num;
    uint8_t bpp;
    uint8_t kern_classes;
    uint8_t bitmap_format;
} lv_font_fmt_txt_dsc_t;

typedef struct _lv_font_t lv_font_t;
struct _lv_font_t {
    bool (*get_glyph_dsc)(const lv_font_t *, void *, uint32_t, uint32_t);
    const uint8_t * (*get_glyph_bitmap)(const lv_font_t *, uint32_t);
    int16_t line_height;
    int16_t base_line;
    uint8_t subpx;
    int8_t underline_position;
    int8_t underline_thickness;
    const void * dsc;
};

static inline bool lv_font_get_glyph_dsc_fmt_txt(
    const lv_font_t * font, void * dsc, uint32_t letter, uint32_t next
) {
    (void)font; (void)dsc; (void)letter; (void)next;
    return false;
}
static inline const uint8_t * lv_font_get_bitmap_fmt_txt(
    const lv_font_t * font, uint32_t letter
) {
    (void)font; (void)letter;
    return 0;
}
#endif
"""


# ---------------------------------------------------------------------------
# Subsetting
# ---------------------------------------------------------------------------


def test_subset_font_preserves_licence_tables_and_requested_coverage(tmp_path):
    TTFont = _font_tools().TTFont
    metadata = ft.FontMetadata.from_path(_sans())
    output = tmp_path / "subset.ttf"

    result = fe.subset_font(
        metadata,
        ["A", "B", "U+0043-U+0044"],
        output,
        flavour="ttf",
    )
    assert result == output
    assert output.exists()
    assert output.stat().st_size < metadata.size_bytes

    font = TTFont(output)
    try:
        cmap = font.getBestCmap()
        assert {0x41, 0x42, 0x43, 0x44}.issubset(cmap)
        assert 0x5A not in cmap
        for table in ("name", "OS/2", "head", "hhea", "maxp", "post", "cmap"):
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


def test_subset_font_is_deterministic(tmp_path):
    metadata = ft.FontMetadata.from_path(_sans())
    first = tmp_path / "first.ttf"
    second = tmp_path / "second.ttf"

    fe.subset_font(metadata, ["A", "B"], first, flavour="ttf")
    fe.subset_font(metadata, ["A", "B"], second, flavour="ttf")

    assert first.read_bytes() == second.read_bytes()


def test_subset_font_refuses_fs_type_without_subsetting(tmp_path):
    for fs_type in (0x0002, 0x0100):
        source = _copy_with_fs_type(tmp_path, fs_type)
        metadata = ft.FontMetadata.from_path(source, license="OFL-1.1")
        with pytest.raises(fe.FontSubsetError):
            fe.subset_font(metadata, ["A"], tmp_path / f"no-{fs_type:04x}.ttf")


def test_subset_font_woff2_uses_system_lib_fallback(
    tmp_path, monkeypatch, woff2_state
):
    monkeypatch.setattr(brotli_fallback, "_python_brotli", lambda: None)
    if brotli_fallback._load_ctypes_libraries() is None:
        pytest.skip("system libbrotlienc/libbrotlidec are not installed")
    sys.modules.pop("brotli", None)
    woff2_state.haveBrotli = False

    metadata = ft.FontMetadata.from_path(_sans())
    output = tmp_path / "subset.woff2"
    fe.subset_font(metadata, ["A", "B"], output, flavour="woff2")

    assert output.read_bytes().startswith(b"wOF2")
    assert output.stat().st_size > 0
    assert woff2_state.haveBrotli is True
    assert getattr(sys.modules["brotli"], "_ozma_ctypes_fallback", False) is True

    TTFont = _font_tools().TTFont
    font = TTFont(output)
    try:
        assert "cmap" in font
        assert {0x41, 0x42}.issubset(font.getBestCmap())
        assert font["OS/2"].fsType == metadata.fs_type
        licence_names = [
            record.toUnicode()
            for record in font["name"].names
            if record.nameID == 13
        ]
        assert any(value.strip() for value in licence_names)
    finally:
        font.close()


def test_subset_font_woff2_fails_closed_when_brotli_hidden(
    tmp_path, monkeypatch, woff2_state
):
    monkeypatch.setattr(brotli_fallback, "_python_brotli", lambda: None)
    monkeypatch.setattr(ctypes.util, "find_library", lambda name: None)
    sys.modules.pop("brotli", None)
    woff2_state.haveBrotli = False

    metadata = ft.FontMetadata.from_path(_sans())
    output = tmp_path / "subset.woff2"
    with pytest.raises(fe.FontExportError, match="brotli"):
        fe.subset_font(metadata, ["A", "B"], output, flavour="woff2")
    assert not output.exists()


# ---------------------------------------------------------------------------
# BDF
# ---------------------------------------------------------------------------


def test_export_bdf_header_properties_and_glyph_shape(tmp_path):
    metadata = ft.FontMetadata.from_path(_sans())
    output = tmp_path / "dejavu.bdf"

    summary = fe.export_bdf(
        metadata, [ord("A"), ord("g")], output, pixel_size=16
    )
    assert summary["format"] == "bdf"
    assert summary["glyphs"] == 2
    assert summary["missing_glyphs"] == []

    text = output.read_text(encoding="utf-8")
    lines = text.splitlines()
    assert lines[0] == "STARTFONT 2.1"
    assert any(line.startswith("FONT ") for line in lines)
    assert any(line.startswith("SIZE 16 ") for line in lines)
    assert "FONT_ASCENT" in text
    assert "FONT_DESCENT" in text
    assert 'FAMILY_NAME "DejaVu Sans"' in text
    assert "CHARS 2" in text
    assert "ENCODING 65" in text
    assert "ENCODING 103" in text
    assert text.rstrip().endswith("ENDFONT")

    # Check that each BBX has a matching, byte-aligned BITMAP block.
    index = 0
    while index < len(lines):
        if lines[index].startswith("BBX "):
            _, width, height, x_offset, y_offset = lines[index].split()
            width = int(width)
            height = int(height)
            assert lines[index + 1] == "BITMAP"
            rows = lines[index + 2 : index + 2 + height]
            assert len(rows) == height
            expected_hex = ((width + 7) // 8) * 2 if width else 0
            for row in rows:
                assert len(row) == expected_hex
            index += 2 + height
        else:
            index += 1


# ---------------------------------------------------------------------------
# LVGL C
# ---------------------------------------------------------------------------


def test_export_lvgl_c_escapes_identifiers_and_writes_sidecar(tmp_path):
    base = ft.FontMetadata.from_path(_sans())
    metadata = ft.FontMetadata(
        **{
            **base.__dict__,
            "family": 'DejaVu "Sans" */ Injection',
            "license": "OFL-1.1 */ license",
            "copyright": "Copyright line one\nCopyright line two",
        }
    )
    output = tmp_path / "font.c"

    summary = fe.export_lvgl_c(
        metadata,
        [ord("A"), ord("B")],
        output,
        bpp=4,
        ranges=["U+0043-U+0044"],
        pixel_size=16,
    )
    assert summary["format"] == "lvgl_c"
    assert summary["glyphs"] == 4
    assert output.exists()
    sidecar = Path(summary["sidecar"])
    assert sidecar.exists()

    text = output.read_text(encoding="utf-8")
    assert "/* Generated by Ozma font_export" in text
    assert "Licence:" in text
    assert "Copyright line one Copyright line two" in text
    assert "* / Injection" in text
    assert 'Sans" */ Injection' not in text
    assert "font_dejavu_sans_injection" in text
    assert "glyph_bitmap_font_dejavu_sans_injection" in text
    assert ".bpp = 4," in text
    assert "LV_FONT_FMT_TXT_CMAP_SPARSE_TINY" in text

    data = json.loads(sidecar.read_text(encoding="utf-8"))
    assert data["family"] == 'DejaVu "Sans" */ Injection'
    assert data["bpp"] == 4
    assert [glyph["codepoint"] for glyph in data["glyphs"]] == [65, 66, 67, 68]
    assert data["glyphs"][0]["bitmap_index"] == 0
    assert data["glyphs"][0]["bytes"] > 0


def test_export_lvgl_c_is_syntactically_compilable(tmp_path):
    gcc = shutil.which("gcc")
    if not gcc:
        pytest.skip("gcc is not available to check generated C")

    metadata = ft.FontMetadata.from_path(_sans())
    output = tmp_path / "font.c"
    fe.export_lvgl_c(metadata, [ord("A"), ord("B")], output, bpp=4, pixel_size=16)
    (tmp_path / "lvgl.h").write_text(_LVGL_HEADER, encoding="utf-8")

    result = subprocess.run(
        [
            gcc,
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-fsyntax-only",
            "-I",
            str(tmp_path),
            str(output),
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr


# ---------------------------------------------------------------------------
# Preview and combined planning
# ---------------------------------------------------------------------------


def test_rasterize_preview_writes_png_and_reports_missing_glyphs(tmp_path):
    metadata = ft.FontMetadata.from_path(_sans())
    output = tmp_path / "preview.png"

    summary = fe.rasterize_preview(metadata, "AB\u4e2d", output, pixel_size=16)
    assert output.exists()
    assert output.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
    assert summary["format"] == "png"
    assert summary["missing_glyphs"] == ["U+4E2D"]


def test_plan_and_export_exact_copies_original(tmp_path):
    metadata = ft.FontMetadata.from_path(_liberation())
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(target="browser", formats=["ttf"])
    output = tmp_path / "exact.ttf"

    report = fe.plan_and_export(
        intent, metadata, [], target, output
    )
    assert report["action"] == "exact"
    assert report["exported"] is True
    assert report["format"] == "ttf"
    assert output.read_bytes() == _liberation().read_bytes()
    assert report["dropped_glyphs"] == []

    jsonschema = pytest.importorskip("jsonschema")
    schema = json.loads(
        (
            Path(__file__).resolve().parents[3]
            / "schemas"
            / "display"
            / "font-v1.schema.json"
        ).read_text(encoding="utf-8")
    )
    jsonschema.validate(report, schema, cls=jsonschema.Draft202012Validator)


def test_plan_and_export_subsets_oversized_source(tmp_path):
    metadata = ft.FontMetadata.from_path(_liberation())
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(
        target="browser", formats=["ttf"], max_bytes=100
    )
    output = tmp_path / "subset.ttf"

    report = fe.plan_and_export(
        intent, metadata, [], target, output, glyphs=["A", "B"]
    )
    assert report["action"] == "subset"
    assert report["exported"] is True
    assert output.exists()

    TTFont = _font_tools().TTFont
    font = TTFont(output)
    try:
        assert {0x41, 0x42}.issubset(font.getBestCmap())
        assert 0x5A not in font.getBestCmap()
    finally:
        font.close()


def test_plan_and_export_reports_and_performs_substitution(tmp_path):
    restricted_path = _copy_with_fs_type(tmp_path, 0x0002, _liberation())
    restricted = ft.FontMetadata.from_path(restricted_path, license="OFL-1.1")
    candidate = ft.FontMetadata.from_path(_liberation())
    intent = ft.FontIntent.new("body", "sans")
    target = ft.TargetFontProfile(target="browser", formats=["ttf"])
    output = tmp_path / "substituted.ttf"

    report = fe.plan_and_export(
        intent, restricted, [candidate], target, output
    )
    assert report["action"] == "substituted"
    assert report["target_font"] == candidate.family
    assert report["source_font"] == restricted.family
    assert report["shape_score"] is not None
    assert report["license_ok"] is True
    assert report["exported"] is True
    assert output.exists()
    assert any("does not permit embedding" in value for value in report["warnings"])
    assert any("substituted" in value for value in report["warnings"])


def test_plan_and_export_rasterizes_for_bdf_target(tmp_path):
    metadata = ft.FontMetadata.from_path(_liberation())
    intent = ft.FontIntent.new("label", "sans")
    target = ft.TargetFontProfile(target="eink", formats=["bdf"], bpp=1)
    output = tmp_path / "panel.bdf"

    report = fe.plan_and_export(
        intent, metadata, [], target, output, glyphs=["A", "B"]
    )
    assert report["action"] == "rasterized"
    assert report["exported"] is True
    assert report["format"] == "bdf"
    assert output.exists()
    assert output.read_text(encoding="utf-8").startswith("STARTFONT 2.1")


def test_plan_and_export_lvgl_target_writes_sidecar(tmp_path):
    metadata = ft.FontMetadata.from_path(_liberation())
    intent = ft.FontIntent.new("label", "sans")
    target = ft.TargetFontProfile(target="lvgl", formats=["lvgl_c"], bpp=4)
    output = tmp_path / "panel.c"

    report = fe.plan_and_export(
        intent,
        metadata,
        [],
        target,
        output,
        glyphs=["A", "B"],
        bpp=4,
        pixel_size=16,
    )
    assert report["action"] == "rasterized"
    assert report["exported"] is True
    assert report["format"] == "lvgl_c"
    assert Path(report["sidecar"]).exists()
    assert output.read_text(encoding="utf-8").startswith("/* Generated by Ozma")
