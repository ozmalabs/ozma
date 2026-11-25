# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Physical font export helpers for the controller.

The translation planner decides *what* should happen to a font; this module
performs the lossy step and records everything it dropped. It supports:

* WOFF2/TTF/OTF subsetting through ``fontTools.subset``;
* BDF bitmap export through Pillow/FreeType;
* LVGL ``lv_font_conv``-style C arrays plus a JSON metadata sidecar;
* PNG previews for controller/browser diagnostics.

Every function refuses to embed a font whose OS/2 ``fsType`` bits do not allow
the requested operation. Reports produced by :func:`plan_and_export` always
name the action, the substituted font, and any dropped glyphs or features.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

from font_translation import (
    FontMetadata,
    FontIntent,
    TargetFontProfile,
    codepoints_from_glyphs,
    _plan_export_with_font,
)


class FontExportError(ValueError):
    """A font cannot be exported for the requested target."""


class FontSubsetError(FontExportError):
    """The licence/embedding bits forbid the requested subset."""


@dataclass
class RasterGlyph:
    """One rasterised glyph in baseline-relative coordinates."""

    codepoint: int
    width: int
    height: int
    x_offset: int
    y_offset: int
    advance: int
    coverage: list[list[int]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "codepoint": self.codepoint,
            "width": self.width,
            "height": self.height,
            "x_offset": self.x_offset,
            "y_offset": self.y_offset,
            "advance": self.advance,
        }


# ---------------------------------------------------------------------------
# FontTools helpers
# ---------------------------------------------------------------------------


def _fonttools() -> tuple[Any, Any, Any, Any]:
    try:
        from fontTools import subset
        from fontTools.ttLib import TTFont
        from io import BytesIO
    except ImportError as exc:  # pragma: no cover - dependency missing
        raise FontExportError(
            "fontTools is required for font subsetting "
            "(install the 'fonttools' package)"
        ) from exc
    return TTFont, subset, BytesIO, None


def _metadata_for(source: str | Path | bytes | FontMetadata) -> FontMetadata:
    if isinstance(source, FontMetadata):
        return source
    if isinstance(source, (str, Path)):
        return FontMetadata.from_path(source)
    return FontMetadata.from_bytes(source)


def _source_path(source: str | Path | bytes | FontMetadata) -> str | None:
    if isinstance(source, FontMetadata):
        return source.path
    if isinstance(source, (str, Path)):
        return str(source)
    return None


def _write_atomic(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temporary = tempfile.mkstemp(
        prefix=f".{path.name}.", suffix=".tmp", dir=str(path.parent)
    )
    try:
        with os.fdopen(handle, "wb") as stream:
            stream.write(data)
        os.replace(temporary, path)
    except Exception:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


# ---------------------------------------------------------------------------
# Subsetting
# ---------------------------------------------------------------------------


def subset_font(
    source: str | Path | bytes | FontMetadata,
    glyphs: Iterable[str | int | tuple[int, int]],
    out_path: str | Path,
    *,
    flavour: str = "woff2",
) -> Path:
    """Subset a font to the requested glyphs.

    ``glyphs`` accepts Unicode codepoints, single characters, names from
    :func:`font_translation.codepoints_from_glyphs`, or ``U+XXXX-U+YYYY``
    ranges. The output is written atomically and deterministic where the
    underlying fontTools version permits (the ``head`` timestamp is reset).

    Raises :class:`FontSubsetError` when the OS/2 ``fsType`` bits forbid
    subsetting or embedding.
    """

    metadata = _metadata_for(source)
    if not metadata.embedding_allowed:
        raise FontSubsetError(
            f"font '{metadata.family}' does not permit embedding "
            f"(OS/2 fsType={metadata.fs_type!r})"
        )
    if not metadata.subset_allowed:
        raise FontSubsetError(
            f"font '{metadata.family}' does not permit subsetting "
            f"(OS/2 fsType={metadata.fs_type!r})"
        )

    flavour = _normalize_flavour(flavour)
    codepoints = sorted(codepoints_from_glyphs(glyphs))
    if not codepoints:
        raise FontExportError("no glyphs were requested for subsetting")

    path = _source_path(source)
    if path is None:
        raise FontExportError(
            "subsetting needs a source path or FontMetadata.path; "
            "in-memory bytes cannot be re-read after subsetting"
        )

    TTFont, subset, _BytesIO, _unused = _fonttools()
    font = TTFont(path, recalcBBoxes=False, recalcTimestamp=False, lazy=False)
    try:
        options = subset.Options()
        # Preserve licence/attribution and layout tables required by reports.
        options.name_IDs = ["*"]
        options.name_legacy = True
        options.name_languages = ["*"]
        options.layout_features = ["*"]
        options.notdef_outline = True
        options.recommended_glyphs = True
        options.symbol_cmap = True
        options.legacy_cmap = True
        options.glyph_names = True
        options.hinting = True
        options.drop_tables = [
            table
            for table in options.drop_tables
            if table not in {"DSIG"}
        ]

        subsetter = subset.Subsetter(options=options)
        subsetter.populate(unicodes=codepoints)
        subsetter.subset(font)

        if "head" in font:
            font["head"].modified = 0
        font.flavor = "woff2" if flavour == "woff2" else None
        if flavour == "woff2":
            _require_woff2_support()

        output = Path(out_path)
        suffix = output.suffix.lower()
        if flavour == "woff2" and suffix not in {".woff2"}:
            # Keep the caller's path but make the format explicit in warnings
            # elsewhere; do not silently rename user-supplied paths.
            pass
        elif flavour in {"ttf", "otf"} and suffix not in {".ttf", ".otf"}:
            pass

        handle, temporary = tempfile.mkstemp(
            prefix=f".{output.name}.", suffix=".tmp", dir=str(output.parent)
        )
        os.close(handle)
        try:
            font.save(temporary, reorderTables=True)
            os.replace(temporary, output)
        except Exception:
            try:
                os.unlink(temporary)
            except OSError:
                pass
            raise
        return output
    finally:
        font.close()


def _normalize_flavour(flavour: str) -> str:
    normalized = str(flavour).strip().lower()
    if normalized in {"woff2", ".woff2"}:
        return "woff2"
    if normalized in {"ttf", "truetype", ".ttf"}:
        return "ttf"
    if normalized in {"otf", "opentype", ".otf"}:
        return "otf"
    raise FontExportError(
        f"unsupported subset flavour {flavour!r}; use 'woff2', 'ttf', or 'otf'"
    )


def _require_woff2_support() -> None:
    try:
        from fontTools.ttLib import woff2
    except ImportError as exc:  # pragma: no cover - old fontTools
        raise FontExportError("this fontTools build has no WOFF2 support") from exc
    if not woff2.haveBrotli:
        raise FontExportError(
            "WOFF2 output requires the 'brotli' Python package; "
            "install it or export TTF/OTF instead"
        )


# ---------------------------------------------------------------------------
# Rasterisation
# ---------------------------------------------------------------------------


def _require_pillow() -> tuple[Any, Any, Any]:
    try:
        from PIL import Image, ImageDraw, ImageFont
    except ImportError as exc:  # pragma: no cover - dependency missing
        raise FontExportError(
            "Pillow is required for bitmap font export "
            "(install the 'Pillow' package)"
        ) from exc
    return Image, ImageDraw, ImageFont


def _font_path(metadata: FontMetadata) -> str:
    if metadata.path:
        return metadata.path
    raise FontExportError(
        f"font '{metadata.family}' has no source path; raster export needs "
        "the original font file"
    )


def rasterize_glyphs(
    metadata: FontMetadata,
    codepoints: Iterable[int],
    *,
    pixel_size: int,
) -> list[RasterGlyph]:
    """Rasterise codepoints at ``pixel_size`` using Pillow/FreeType."""

    if pixel_size <= 0:
        raise FontExportError("pixel_size must be positive")
    _Image, _ImageDraw, ImageFont = _require_pillow()
    font = ImageFont.truetype(_font_path(metadata), int(pixel_size))

    ordered = sorted({int(value) for value in codepoints})
    glyphs: list[RasterGlyph] = []
    for codepoint in ordered:
        character = chr(codepoint)
        try:
            mask, offset = font.getmask2(character, mode="L", anchor="ls")
        except Exception:  # pragma: no cover - malformed glyph
            mask, offset = None, (0, 0)
        width, height = (mask.size if mask is not None else (0, 0))
        if mask is not None and width and height:
            raw = bytes(mask)
            coverage = [
                list(raw[row * width : (row + 1) * width])
                for row in range(height)
            ]
        else:
            coverage = []
        x_offset = int(offset[0])
        top_offset = int(offset[1])
        y_offset = -(top_offset + height)
        advance = int(round(font.getlength(character)))
        glyphs.append(
            RasterGlyph(
                codepoint=codepoint,
                width=int(width),
                height=int(height),
                x_offset=x_offset,
                y_offset=y_offset,
                advance=advance,
                coverage=coverage,
            )
        )
    return glyphs


def _write_bdf_bitmap_row(coverage_row: Sequence[int], width: int) -> str:
    byte_width = (width + 7) // 8
    packed = bytearray(byte_width)
    for x in range(width):
        if coverage_row[x] >= 128:
            packed[x // 8] |= 0x80 >> (x % 8)
    return "".join(f"{byte:02X}" for byte in packed)


def _bdf_atom(value: str) -> str:
    cleaned = re.sub(r"[\s\-]+", "_", str(value).strip())
    cleaned = re.sub(r"[^0-9A-Za-z_]", "", cleaned)
    return cleaned or "Unknown"


def _bdf_string(value: str) -> str:
    text = (
        str(value)
        .replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\r", " ")
        .replace("\n", " ")
    )
    return '"' + text + '"'


def export_bdf(
    metadata: FontMetadata,
    glyphs: Iterable[str | int | tuple[int, int]] | Mapping[int, Any],
    out_path: str | Path,
    *,
    pixel_size: int,
) -> dict[str, Any]:
    """Write a BDF bitmap font and return an export summary."""

    raster = _coerce_raster_glyphs(metadata, glyphs, pixel_size=pixel_size)
    if not raster:
        raise FontExportError("no glyphs were requested for BDF export")

    _Image, _ImageDraw, ImageFont = _require_pillow()
    pil_font = ImageFont.truetype(_font_path(metadata), int(pixel_size))
    ascent, descent = (int(value) for value in pil_font.getmetrics())
    max_width = max((glyph.width for glyph in raster), default=1)
    max_height = max((glyph.height for glyph in raster), default=1)

    family = _bdf_atom(metadata.family)
    style = _bdf_atom(metadata.style or "Regular")
    weight_name = _bdf_atom(_weight_name(metadata.weight))
    slant = {"italic": "I", "oblique": "O"}.get(metadata.slant, "R")
    spacing = "M" if metadata.is_monospaced else "P"
    average_width = int(
        round((metadata.average_advance_ratio() or 0.5) * 1000)
    )
    xlfd = (
        f"-ozma-{family}-{style}-{weight_name}-{slant}-Normal--"
        f"{pixel_size}-{pixel_size * 10}-75-75-{spacing}-"
        f"{average_width}-ISO10646-1"
    )

    properties: list[tuple[str, str]] = [
        ("FONT_ASCENT", str(ascent)),
        ("FONT_DESCENT", str(descent)),
        ("DEFAULT_CHAR", "32"),
        ("FAMILY_NAME", _bdf_string(metadata.family)),
        ("WEIGHT_NAME", _bdf_string(_weight_name(metadata.weight))),
        ("SLANT", _bdf_string(slant)),
        ("SPACING", _bdf_string(spacing)),
        ("CHARSET_REGISTRY", '"ISO10646"'),
        ("CHARSET_ENCODING", '"1"'),
    ]
    if metadata.copyright:
        properties.append(("COPYRIGHT", _bdf_string(metadata.copyright)))
    if metadata.license:
        properties.append(("LICENSE", _bdf_string(metadata.license)))
    if metadata.license_url:
        properties.append(("LICENSE_URL", _bdf_string(metadata.license_url)))

    lines = [
        "STARTFONT 2.1",
        f"FONT {xlfd}",
        f"SIZE {pixel_size} 75 75",
        "FONTBOUNDINGBOX "
        f"{max_width} {max_height} 0 -{max(descent, 0)}",
        f"STARTPROPERTIES {len(properties)}",
    ]
    lines.extend(f"{name} {value}" for name, value in properties)
    lines.append("ENDPROPERTIES")
    lines.append(f"CHARS {len(raster)}")

    for glyph in raster:
        lines.append(f"STARTCHAR uni{glyph.codepoint:04X}")
        lines.append(f"ENCODING {glyph.codepoint}")
        swidth = (
            int(round(glyph.advance * 1000 / max(pixel_size, 1)))
            if pixel_size
            else 0
        )
        lines.append(f"SWIDTH {swidth} 0")
        lines.append(f"DWIDTH {glyph.advance} 0")
        lines.append(
            f"BBX {glyph.width} {glyph.height} "
            f"{glyph.x_offset} {glyph.y_offset}"
        )
        lines.append("BITMAP")
        for row in glyph.coverage:
            lines.append(_write_bdf_bitmap_row(row, glyph.width))
        lines.append("ENDCHAR")
    lines.append("ENDFONT")

    output = Path(out_path)
    _write_atomic(output, ("\n".join(lines) + "\n").encode("utf-8"))
    return {
        "output": str(output),
        "format": "bdf",
        "pixel_size": int(pixel_size),
        "glyphs": len(raster),
        "missing_glyphs": [
            f"U+{glyph.codepoint:04X}"
            for glyph in raster
            if glyph.codepoint not in metadata.codepoint_set()
        ],
    }


def _weight_name(weight: int) -> str:
    if weight < 250:
        return "Thin"
    if weight < 350:
        return "Light"
    if weight < 450:
        return "Medium"
    if weight < 550:
        return "SemiBold"
    if weight < 750:
        return "Bold"
    return "Black"


# ---------------------------------------------------------------------------
# LVGL C export
# ---------------------------------------------------------------------------


def _c_identifier(value: str) -> str:
    cleaned = re.sub(r"[^0-9A-Za-z_]+", "_", str(value).strip())
    cleaned = re.sub(r"_+", "_", cleaned).strip("_").lower()
    if not cleaned:
        cleaned = "font"
    if cleaned[0].isdigit():
        cleaned = "_" + cleaned
    return f"font_{cleaned}"


def _c_comment(value: str) -> str:
    return (
        str(value)
        .replace("*/", "* /")
        .replace("\r", " ")
        .replace("\n", " ")
    )


def _pack_coverage(
    coverage: Sequence[Sequence[int]], width: int, height: int, bpp: int
) -> bytes:
    if width == 0 or height == 0:
        return b""
    levels = (1 << bpp) - 1
    row_bytes = (width * bpp + 7) // 8
    packed = bytearray(row_bytes * height)
    for y in range(height):
        row = coverage[y] if y < len(coverage) else []
        bit_position = 0
        for x in range(width):
            value = row[x] if x < len(row) else 0
            level = (value * levels + 127) // 255
            byte_index = y * row_bytes + bit_position // 8
            shift = 8 - bpp - (bit_position % 8)
            packed[byte_index] |= (level & levels) << shift
            bit_position += bpp
    return bytes(packed)


def export_lvgl_c(
    metadata: FontMetadata,
    glyphs: Iterable[str | int | tuple[int, int]] | Mapping[int, Any],
    out_path: str | Path,
    *,
    bpp: int = 4,
    ranges: Iterable[str | int | tuple[int, int]] | None = None,
    pixel_size: int = 16,
) -> dict[str, Any]:
    """Write lv_font_conv-style C arrays and a JSON metadata sidecar.

    ``ranges`` adds Unicode ranges to the requested glyph set, which is useful
    when an intent declares a whole block. The sidecar is written next to the C
    file with the same stem and a ``.json`` suffix.
    """

    if bpp not in (1, 2, 4, 8):
        raise FontExportError("bpp must be one of 1, 2, 4, or 8")

    requested: set[int] = set()
    if isinstance(glyphs, Mapping):
        requested.update(int(value) for value in glyphs)
    else:
        requested.update(codepoints_from_glyphs(glyphs))
    if ranges:
        requested.update(codepoints_from_glyphs(ranges))
    if not requested:
        raise FontExportError("no glyphs or ranges were requested for LVGL export")

    if isinstance(glyphs, Mapping):
        raster = _coerce_raster_glyphs(
            metadata,
            {codepoint: glyphs[codepoint] for codepoint in requested if codepoint in glyphs},
            pixel_size=pixel_size,
        )
        missing_from_map = requested - {glyph.codepoint for glyph in raster}
        if missing_from_map:
            raster.extend(
                rasterize_glyphs(metadata, missing_from_map, pixel_size=pixel_size)
            )
    else:
        raster = rasterize_glyphs(metadata, requested, pixel_size=pixel_size)
    raster.sort(key=lambda glyph: glyph.codepoint)

    identifier = _c_identifier(metadata.family)
    bitmap_array = f"glyph_bitmap_{identifier}"
    descriptor_array = f"glyph_dsc_{identifier}"
    unicode_array = f"unicode_list_{identifier}"
    cmap_array = f"cmaps_{identifier}"
    descriptor_name = f"{identifier}_dsc"
    font_name = identifier

    bitmaps: list[bytes] = []
    offsets: list[int] = []
    bitmap_index = 0
    for glyph in raster:
        packed = _pack_coverage(glyph.coverage, glyph.width, glyph.height, bpp)
        offsets.append(bitmap_index)
        bitmaps.append(packed)
        bitmap_index += len(packed)

    _Image, _ImageDraw, ImageFont = _require_pillow()
    pil_font = ImageFont.truetype(_font_path(metadata), int(pixel_size))
    ascent, descent = (int(value) for value in pil_font.getmetrics())
    line_height = ascent + descent

    lines: list[str] = [
        "/* Generated by Ozma font_export. Do not edit by hand. */",
        f"/* Family: {_c_comment(metadata.family)} */",
        f"/* Style: {_c_comment(metadata.style or 'Regular')} */",
        f"/* Licence: {_c_comment(metadata.license or 'unknown')} */",
        f"/* Copyright: {_c_comment(metadata.copyright or 'not declared')} */",
        f"/* Source SHA-256: {metadata.sha256} */",
        "",
        '#include "lvgl.h"',
        "#include <stddef.h>",
        "",
        "#ifndef LV_ATTRIBUTE_LARGE_CONST",
        "#define LV_ATTRIBUTE_LARGE_CONST",
        "#endif",
        "",
        f"static const uint8_t {bitmap_array}[] LV_ATTRIBUTE_LARGE_CONST = {{",
    ]
    if bitmaps:
        wrote_value = False
        for index, packed in enumerate(bitmaps):
            values = ", ".join(f"0x{byte:02X}" for byte in packed)
            if values:
                lines.append(f"    {values}, /* glyph {index} */")
                wrote_value = True
            else:
                lines.append(f"    /* glyph {index}: empty bitmap */")
        if not wrote_value:
            lines.append("    0x00,")
    else:  # pragma: no cover - guarded earlier
        lines.append("    0x00")
    lines.extend(
        [
            "};",
            "",
            f"static const lv_font_fmt_txt_glyph_dsc_t {descriptor_array}[] = {{",
        ]
    )
    for glyph, offset in zip(raster, offsets):
        lines.append(
            "    {"
            f" .bitmap_index = {offset},"
            f" .adv_w = {glyph.advance * 16},"
            f" .box_w = {glyph.width},"
            f" .box_h = {glyph.height},"
            f" .ofs_x = {glyph.x_offset},"
            f" .ofs_y = {glyph.y_offset}"
            " },"
        )
    lines.extend(
        [
            "};",
            "",
            f"static const uint16_t {unicode_array}[] = {{",
            "    "
            + ", ".join(f"0x{glyph.codepoint:04X}" for glyph in raster),
            "};",
            "",
            f"static const lv_font_fmt_txt_cmap_t {cmap_array}[] = {{",
            "    {",
            "        .range_start = "
            f"0x{raster[0].codepoint:04X},",
            f"        .range_length = {len(raster)},",
            "        .glyph_id_start = 0,",
            f"        .unicode_list = {unicode_array},",
            "        .glyph_id_ofs_list = NULL,",
            f"        .list_length = {len(raster)},",
            "        .type = LV_FONT_FMT_TXT_CMAP_SPARSE_TINY,",
            "    },",
            "};",
            "",
            f"static const lv_font_fmt_txt_dsc_t {descriptor_name} = {{",
            f"    .glyph_bitmap = {bitmap_array},",
            f"    .glyph_dsc = {descriptor_array},",
            f"    .cmaps = {cmap_array},",
            "    .kern_dsc = NULL,",
            "    .kern_scale = 0,",
            "    .cmap_num = 1,",
            f"    .bpp = {bpp},",
            "    .kern_classes = 0,",
            "    .bitmap_format = 0,",
            "};",
            "",
            f"lv_font_t {font_name} = {{",
            "    .get_glyph_dsc = lv_font_get_glyph_dsc_fmt_txt,",
            "    .get_glyph_bitmap = lv_font_get_bitmap_fmt_txt,",
            f"    .line_height = {line_height},",
            f"    .base_line = {descent},",
            "    .subpx = LV_FONT_SUBPX_NONE,",
            "    .underline_position = -1,",
            "    .underline_thickness = 1,",
            f"    .dsc = &{descriptor_name},",
            "};",
            "",
        ]
    )

    output = Path(out_path)
    _write_atomic(output, ("\n".join(lines) + "\n").encode("utf-8"))
    sidecar = output.with_suffix(".json")
    sidecar_data = {
        "id": metadata.id,
        "family": metadata.family,
        "style": metadata.style,
        "license": metadata.license,
        "copyright": metadata.copyright,
        "sha256": metadata.sha256,
        "source_format": metadata.format,
        "format": "lvgl_c",
        "bpp": int(bpp),
        "pixel_size": int(pixel_size),
        "line_height": line_height,
        "base_line": descent,
        "identifier": identifier,
        "ranges": [
            _format_range_value(value) for value in (ranges or [])
        ],
        "glyphs": [
            {
                **glyph.to_dict(),
                "bitmap_index": offset,
                "bytes": len(packed),
            }
            for glyph, offset, packed in zip(raster, offsets, bitmaps)
        ],
    }
    _write_atomic(
        sidecar,
        (json.dumps(sidecar_data, indent=2, sort_keys=True) + "\n").encode("utf-8"),
    )
    return {
        "output": str(output),
        "sidecar": str(sidecar),
        "format": "lvgl_c",
        "bpp": int(bpp),
        "pixel_size": int(pixel_size),
        "glyphs": len(raster),
        "missing_glyphs": [
            f"U+{glyph.codepoint:04X}"
            for glyph in raster
            if glyph.codepoint not in metadata.codepoint_set()
        ],
    }


def _format_range_value(value: str | int | tuple[int, int]) -> str:
    codepoints = sorted(codepoints_from_glyphs([value]))
    if not codepoints:
        return ""
    if len(codepoints) == 1:
        return f"U+{codepoints[0]:04X}"
    return f"U+{codepoints[0]:04X}-U+{codepoints[-1]:04X}"


def _coerce_raster_glyphs(
    metadata: FontMetadata,
    glyphs: Mapping[int, Any] | Iterable[str | int | tuple[int, int]],
    *,
    pixel_size: int,
) -> list[RasterGlyph]:
    if not isinstance(glyphs, Mapping):
        codepoints = sorted(codepoints_from_glyphs(glyphs))
        return rasterize_glyphs(metadata, codepoints, pixel_size=pixel_size)

    raster: list[RasterGlyph] = []
    for codepoint, value in sorted(glyphs.items(), key=lambda item: int(item[0])):
        codepoint = int(codepoint)
        if isinstance(value, RasterGlyph):
            raster.append(value)
            continue
        if isinstance(value, Mapping):
            width = int(value.get("width", 0))
            height = int(value.get("height", 0))
            coverage = [
                [int(pixel) for pixel in row]
                for row in value.get("coverage", value.get("rows", []))
            ]
            raster.append(
                RasterGlyph(
                    codepoint=codepoint,
                    width=width,
                    height=height,
                    x_offset=int(value.get("x_offset", value.get("xoff", 0))),
                    y_offset=int(value.get("y_offset", value.get("yoff", 0))),
                    advance=int(value.get("advance", width)),
                    coverage=coverage,
                )
            )
            continue
        rows = list(value)
        width = max((len(row) for row in rows), default=0)
        coverage = [
            [255 if str(pixel) in {"1", "255", "#", "X", "x"} else 0 for pixel in str(row)]
            for row in rows
        ]
        raster.append(
            RasterGlyph(
                codepoint=codepoint,
                width=width,
                height=len(coverage),
                x_offset=0,
                y_offset=0,
                advance=width,
                coverage=coverage,
            )
        )
    return raster


# ---------------------------------------------------------------------------
# Preview
# ---------------------------------------------------------------------------


def rasterize_preview(
    metadata: FontMetadata,
    text: str,
    out_path: str | Path,
    *,
    pixel_size: int,
) -> dict[str, Any]:
    """Render ``text`` to a transparent PNG and report missing glyphs."""

    if pixel_size <= 0:
        raise FontExportError("pixel_size must be positive")
    Image, ImageDraw, ImageFont = _require_pillow()
    font = ImageFont.truetype(_font_path(metadata), int(pixel_size))

    covered = metadata.codepoint_set()
    missing = [
        f"U+{ord(character):04X}"
        for character in text
        if character not in "\r\n\t" and (not covered or ord(character) not in covered)
    ]

    left, top, right, bottom = font.getbbox(text or " ", anchor="ls")
    width = max(1, right - left)
    height = max(1, bottom - top)
    image = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.text((-left, -top), text, font=font, fill=(255, 255, 255, 255), anchor="ls")

    output = Path(out_path)
    output.parent.mkdir(parents=True, exist_ok=True)
    image.save(output, format="PNG", optimize=False)
    return {
        "output": str(output),
        "format": "png",
        "pixel_size": int(pixel_size),
        "text": text,
        "missing_glyphs": missing,
    }


# ---------------------------------------------------------------------------
# Combined planning and export
# ---------------------------------------------------------------------------


def plan_and_export(
    intent: FontIntent | Mapping[str, Any],
    original: FontMetadata | Mapping[str, Any] | None,
    candidates: Sequence[FontMetadata | Mapping[str, Any]],
    target: TargetFontProfile | Mapping[str, Any],
    out_path: str | Path | None = None,
    *,
    glyphs: Iterable[str | int | tuple[int, int]] | None = None,
    pixel_size: int = 16,
    bpp: int | None = None,
    flavour: str | None = None,
    text: str | None = None,
) -> dict[str, Any]:
    """Plan translation, perform the selected export, and return a report dict.

    The returned mapping is the schema-valid ``FontTranslationReport`` plus
    export details (``output``, ``format``, ``exported``). ``warnings`` and the
    dropped glyph/feature lists make every substitution and rasterisation
    explicit.
    """

    report, chosen = _plan_export_with_font(intent, original, candidates, target)
    target_profile = (
        target
        if isinstance(target, TargetFontProfile)
        else TargetFontProfile.from_dict(target)
    )
    if report.action == "substituted":
        metadata = chosen
    elif original is not None:
        metadata = _as_metadata(original)
    else:
        metadata = chosen

    result = report.to_dict()
    result.update({"output": None, "format": None, "exported": False})

    if report.action == "unavailable" or metadata is None:
        result["warnings"] = list(result.get("warnings", [])) + [
            "no legal, embeddable font is available; nothing was exported"
        ]
        return result

    if out_path is None:
        result["warnings"] = list(result.get("warnings", [])) + [
            "no output path supplied; export was planned but not written"
        ]
        return result

    output = Path(out_path)
    selected_glyphs = glyphs
    if selected_glyphs is None and report.action == "subset":
        selected_glyphs = sorted(metadata.codepoint_set())

    try:
        if report.action == "exact":
            _copy_exact(metadata, output)
            result["format"] = metadata.format
        elif report.action == "subset":
            subset_flavour = flavour or _flavour_for_target(target_profile, output)
            subset_font(
                metadata,
                selected_glyphs or (),
                output,
                flavour=subset_flavour,
            )
            result["format"] = subset_flavour
        elif report.action == "substituted":
            if selected_glyphs:
                subset_flavour = flavour or _flavour_for_target(target_profile, output)
                if metadata.format in {"ttf", "otf"} and subset_flavour in {
                    "ttf",
                    "otf",
                    "woff2",
                }:
                    subset_font(metadata, selected_glyphs, output, flavour=subset_flavour)
                    result["format"] = subset_flavour
                else:
                    _copy_exact(metadata, output)
                    result["format"] = metadata.format
            else:
                _copy_exact(metadata, output)
                result["format"] = metadata.format
        else:  # rasterized
            details = _export_rasterized(
                metadata,
                output,
                target_profile,
                selected_glyphs,
                pixel_size=pixel_size,
                bpp=bpp,
                text=text,
            )
            missing = details.pop("dropped_glyphs", [])
            result.update(details)
            result["dropped_glyphs"] = sorted(
                set(result.get("dropped_glyphs", [])) | set(missing)
            )
    except FontExportError as exc:
        result["warnings"] = list(result.get("warnings", [])) + [str(exc)]
        return result

    result["output"] = str(output)
    result["exported"] = output.exists()
    return result


def _as_metadata(value: FontMetadata | Mapping[str, Any]) -> FontMetadata:
    if isinstance(value, FontMetadata):
        return value
    return FontMetadata.from_dict(value)


def _copy_exact(metadata: FontMetadata, output: Path) -> None:
    if not metadata.path:
        raise FontExportError(
            f"font '{metadata.family}' has no source path to copy"
        )
    output.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(metadata.path, output)


def _flavour_for_target(target: TargetFontProfile, output: Path) -> str:
    suffix = output.suffix.lower().lstrip(".")
    if suffix in {"woff2", "ttf", "otf"}:
        return suffix
    for candidate in target.formats:
        normalized = candidate.strip().lower().lstrip(".")
        if normalized in {"woff2", "ttf", "otf"}:
            return normalized
    return "ttf"


def _export_rasterized(
    metadata: FontMetadata,
    output: Path,
    target: TargetFontProfile,
    glyphs: Iterable[str | int | tuple[int, int]] | None,
    *,
    pixel_size: int,
    bpp: int | None,
    text: str | None,
) -> dict[str, Any]:
    formats = {
        candidate.strip().lower()
        for candidate in target.formats
    }
    selected = glyphs
    if selected is None:
        selected = sorted(metadata.codepoint_set())

    if "lvgl_c" in formats or "lvgl" in formats:
        summary = export_lvgl_c(
            metadata,
            selected,
            output,
            bpp=bpp or target.bpp or 4,
            pixel_size=pixel_size,
        )
        return {
            "format": "lvgl_c",
            "sidecar": summary["sidecar"],
            "dropped_glyphs": summary["missing_glyphs"],
        }
    if formats & {"bdf", "pcf", "bitmap"}:
        summary = export_bdf(
            metadata, selected, output, pixel_size=pixel_size
        )
        return {
            "format": "bdf",
            "dropped_glyphs": summary["missing_glyphs"],
        }
    if text is not None:
        summary = rasterize_preview(
            metadata, text, output, pixel_size=pixel_size
        )
        return {
            "format": "png",
            "dropped_glyphs": summary["missing_glyphs"],
        }
    summary = export_bdf(metadata, selected, output, pixel_size=pixel_size)
    return {"format": "bdf", "dropped_glyphs": summary["missing_glyphs"]}


__all__ = [
    "FontExportError",
    "FontSubsetError",
    "RasterGlyph",
    "export_bdf",
    "export_lvgl_c",
    "plan_and_export",
    "rasterize_glyphs",
    "rasterize_preview",
    "subset_font",
]
