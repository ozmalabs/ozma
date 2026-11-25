# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Terminal/BIOS text OCR via bitmap template matching.

Recognises text on captured display frames using fixed-width font templates
— no ML, no Tesseract, no cloud API.  Works on BIOS screens, DOS, Linux
console, grub, UEFI shell, and any terminal using standard monospaced fonts.

How it works:

  1. Captured frame (from display_capture.py) → grayscale
  2. Detect character grid: find cell size by autocorrelation of row/column
     intensity patterns (most terminal displays have a regular grid)
  3. Extract each cell as an NxM pixel region
  4. Compare against embedded font bitmaps using normalised correlation
  5. Best match → character code → UTF-8 text

Embedded fonts:
  - VGA 8x16 (CP437) — BIOS, DOS, Linux console, GRUB, UEFI shell
  - VGA 8x14 — some BIOS/DOS modes
  - VGA 9x16 — VGA text mode with 9th column

The font data is ~4KB per font (256 glyphs × 16 bytes).  Template matching
runs at >1000 characters/ms on modern hardware — a full 80×25 screen in <2ms.

API:
  POST /api/v1/captures/{id}/ocr    → extract text from current frame
  POST /api/v1/captures/{id}/ocr/region → extract text from a region
  GET  /api/v1/captures/{id}/text   → cached text from last OCR run

Use cases:
  - Copy text from a BIOS screen (no clipboard, no OS running)
  - Read IP addresses from a Linux console during boot
  - Copy error messages from a blue screen / kernel panic
  - Automated monitoring: watch for specific text patterns
"""

from __future__ import annotations

import logging
import numpy as np
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.text_capture")

try:
    from PIL import Image
    _PIL_AVAILABLE = True
except ImportError:
    _PIL_AVAILABLE = False


# ── VGA 8x16 CP437 font ─────────────────────────────────────────────────────
#
# Standard VGA text mode font.  256 glyphs, 8 pixels wide, 16 pixels tall.
# Each glyph is 16 bytes (one byte per row, MSB = leftmost pixel).
# This is the font burned into every VGA-compatible BIOS since 1987.
#
# Only printable ASCII subset embedded here (32-126) plus key box-drawing
# characters.  Full CP437 can be loaded from system PSF fonts at runtime.

# CP437 → Unicode mapping for printable range + common box-drawing
_CP437_MAP = {i: chr(i) for i in range(32, 127)}
_CP437_MAP.update({
    0: " ", 1: "\u263a", 2: "\u263b", 3: "\u2665", 4: "\u2666", 5: "\u2663", 6: "\u2660",
    7: "\u2022", 8: "\u25d8", 9: "\u25cb", 10: "\u25d9", 11: "\u2642", 12: "\u2640",
    13: "\u266a", 14: "\u266b", 15: "\u263c", 16: "\u25ba", 17: "\u25c4", 18: "\u2195",
    19: "\u203c", 20: "\u00b6", 21: "\u00a7", 22: "\u25ac", 23: "\u21a8", 24: "\u2191",
    25: "\u2193", 26: "\u2192", 27: "\u2190", 28: "\u221f", 29: "\u2194", 30: "\u25b2",
    31: "\u25bc", 127: "\u2302",
    176: "\u2591", 177: "\u2592", 178: "\u2593", 179: "\u2502", 180: "\u2524",
    186: "\u2551", 187: "\u2557", 188: "\u255d", 191: "\u2510", 192: "\u2514",
    193: "\u2534", 194: "\u252c", 195: "\u251c", 196: "\u2500", 197: "\u253c",
    200: "\u255a", 201: "\u2554", 202: "\u2569", 203: "\u2566", 204: "\u2560",
    205: "\u2550", 206: "\u256c", 217: "\u2518", 218: "\u250c",
    219: "\u2588", 220: "\u2584", 223: "\u2580",
    254: "\u25a0", 255: " ",
})


@dataclass
class FontTemplate:
    """A bitmap font used for template matching."""

    name: str
    width: int             # pixels per glyph
    height: int            # pixels per glyph
    glyphs: dict[int, np.ndarray] = field(default_factory=dict)  # code → (height, width) bool array
    #: True for a real console/BIOS bitmap font, False for the Pillow
    #: approximation generated when no system PSF font is available.
    is_native: bool = True

    @classmethod
    def from_raw_bytes(cls, name: str, data: bytes, width: int = 8, height: int = 16, num_glyphs: int = 256) -> "FontTemplate":
        """Parse raw VGA font data (1 byte per row, MSB-first)."""
        font = cls(name=name, width=width, height=height)
        for code in range(min(num_glyphs, len(data) // height)):
            rows = []
            for row in range(height):
                byte = data[code * height + row]
                bits = [(byte >> (width - 1 - col)) & 1 for col in range(width)]
                rows.append(bits)
            font.glyphs[code] = np.array(rows, dtype=np.float32)
        return font

    @classmethod
    def from_psf_file(cls, path: str) -> "FontTemplate":
        """Load a PSF (PC Screen Font) file."""
        import gzip
        raw = open(path, "rb").read() if not path.endswith(".gz") else gzip.open(path, "rb").read()

        # PSF2 magic: 0x72 0xb5 0x4a 0x86
        if raw[:4] == b"\x72\xb5\x4a\x86":
            header_size = int.from_bytes(raw[8:12], "little")
            num_glyphs = int.from_bytes(raw[16:20], "little")
            glyph_bytes = int.from_bytes(raw[20:24], "little")
            height = int.from_bytes(raw[24:28], "little")
            width = int.from_bytes(raw[28:32], "little")
            data = raw[header_size:]
            return cls.from_raw_bytes(f"PSF2:{path}", data, width, height, num_glyphs)

        # PSF1 magic: 0x36 0x04
        if raw[:2] == b"\x36\x04":
            mode = raw[2]
            glyph_bytes = raw[3]
            num_glyphs = 512 if (mode & 0x01) else 256
            data = raw[4:]
            return cls.from_raw_bytes(f"PSF1:{path}", data, 8, glyph_bytes, num_glyphs)

        raise ValueError(f"Not a PSF font: {path}")


@dataclass
class OCRResult:
    """Result of text recognition on a frame."""

    text: str                       # Recognised text (lines joined by \n)
    lines: list[str]                # Individual lines
    grid_width: int = 0             # Characters per line
    grid_height: int = 0            # Number of lines
    cell_width: int = 0             # Pixel width per character cell
    cell_height: int = 0            # Pixel height per character cell
    confidence: float = 0.0         # Average best-match correlation (0-1)
    cells: list[list[dict]] = field(default_factory=list)  # Per-cell info
    #: Average gap between the best and second-best glyph correlation over
    #: non-blank cells.  Unlike raw cosine correlation this is discriminative:
    #: arbitrary GUI pixels tend to match many glyphs almost equally well.
    match_margin: float = 0.0
    #: True when a real console font was used (not the Pillow approximation).
    font_native: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "text": self.text,
            "lines": self.lines,
            "grid": f"{self.grid_width}x{self.grid_height}",
            "cell_size": f"{self.cell_width}x{self.cell_height}",
            "confidence": round(self.confidence, 3),
        }


class TextCapture:
    """
    OCR engine for terminal/BIOS text displays.

    Uses bitmap template matching against embedded VGA fonts.
    No external OCR engine required.
    """

    def __init__(self) -> None:
        self._fonts: list[FontTemplate] = []
        self._last_result: OCRResult | None = None
        self._load_system_fonts()

    def _load_system_fonts(self) -> None:
        """Load available VGA/console fonts from the system."""
        from pathlib import Path

        # Debian/Ubuntu keep console fonts in /usr/share/consolefonts, while
        # Arch/Fedora and the kbd package use /usr/share/kbd/consolefonts.
        # Missing the latter made TextCapture fall back to an anti-aliased
        # Pillow font, which cannot match a real VGA console.
        psf_dirs = [
            Path("/usr/share/consolefonts"),
            Path("/usr/lib/kbd/consolefonts"),
            Path("/usr/share/kbd/consolefonts"),
            Path("/usr/local/share/consolefonts"),
        ]
        # Base names (no extension) of fonts that closely match the classic
        # VGA 8x16/8x14 text-mode glyphs.
        preferred = [
            "Lat2-VGA16",
            "default8x16",
            "VGA-ROM",
            "LatGrkCyr-8x16",
            "UniCyr_8x16",
            "Mik_8x16",
            "cp850-8x16",
            "Lat15-VGA16",
        ]
        suffixes = ["", ".psf", ".psfu", ".gz", ".psf.gz", ".psfu.gz", ".f16"]

        for psf_dir in psf_dirs:
            if not psf_dir.exists():
                continue
            for name in preferred:
                for suffix in suffixes:
                    path = psf_dir / (name + suffix)
                    if path.exists():
                        try:
                            font = FontTemplate.from_psf_file(str(path))
                            self._fonts.append(font)
                            log.info("Loaded font: %s (%dx%d, %d glyphs)",
                                     font.name, font.width, font.height, len(font.glyphs))
                            return  # One font is enough
                        except Exception as e:
                            log.debug("Failed to load %s: %s", path, e)

            # Last resort: try any PSF font in the directory.  Some distros
            # only ship locale-specific names that are still valid VGA fonts.
            for path in sorted(psf_dir.iterdir()):
                if not path.is_file():
                    continue
                lname = path.name.lower()
                if not (".psf" in lname or lname.endswith(".gz") or lname.endswith(".f16")):
                    continue
                try:
                    font = FontTemplate.from_psf_file(str(path))
                    if font.width in (8, 9) and font.height in (8, 14, 16):
                        self._fonts.append(font)
                        log.info("Loaded fallback font: %s (%dx%d, %d glyphs)",
                                 font.name, font.width, font.height, len(font.glyphs))
                        return
                except Exception as e:
                    log.debug("Failed to load %s: %s", path, e)

        # Fallback: generate a minimal ASCII font from Pillow.  This is only an
        # approximation, so callers must not treat its matches as high
        # confidence (see OCRResult.font_native).
        if _PIL_AVAILABLE and not self._fonts:
            self._generate_pillow_font()

    def _generate_pillow_font(self) -> None:
        """Generate font templates by rendering with Pillow."""
        from PIL import ImageFont, ImageDraw, Image
        try:
            font_obj = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 14)
        except (OSError, IOError):
            font_obj = ImageFont.load_default()

        width, height = 8, 16
        ft = FontTemplate(name="pillow-mono", width=width, height=height, is_native=False)

        for code in range(32, 127):
            img = Image.new("L", (width, height), 0)
            draw = ImageDraw.Draw(img)
            draw.text((0, 0), chr(code), fill=255, font=font_obj)
            ft.glyphs[code] = np.array(img, dtype=np.float32) / 255.0

        self._fonts.append(ft)
        log.info("Generated Pillow font templates (8x16, %d glyphs)", len(ft.glyphs))

    def recognise_frame(
        self,
        frame: Image.Image | np.ndarray,
        region: tuple[int, int, int, int] | None = None,
    ) -> OCRResult:
        """
        Recognise text in a captured frame.

        Args:
            frame: PIL Image or numpy array (RGB or grayscale)
            region: Optional (x1, y1, x2, y2) crop region

        Returns:
            OCRResult with recognised text
        """
        if not self._fonts:
            return OCRResult(text="", lines=[], confidence=0.0)

        # Convert to grayscale numpy array
        if isinstance(frame, Image.Image):
            if region:
                frame = frame.crop(region)
            gray = np.array(frame.convert("L"), dtype=np.float32) / 255.0
        else:
            if region:
                x1, y1, x2, y2 = region
                frame = frame[y1:y2, x1:x2]
            if len(frame.shape) == 3:
                # Rec. 601 luminance, ignoring any alpha channel.  A plain
                # channel mean shifts coloured/subpixel-antialiased console
                # text and made the cell detector pick a half-size grid.
                rgb = frame[:, :, :3].astype(np.float32)
                gray = (0.299 * rgb[:, :, 0]
                        + 0.587 * rgb[:, :, 1]
                        + 0.114 * rgb[:, :, 2]) / 255.0
            else:
                gray = frame.astype(np.float32) / 255.0

        font = self._fonts[0]
        h, w = gray.shape

        # Detect cell size — try the font's native size first
        cell_w, cell_h = self._detect_cell_size(gray, font)

        # Grid dimensions
        cols = w // cell_w if cell_w > 0 else 0
        rows = h // cell_h if cell_h > 0 else 0

        if cols == 0 or rows == 0:
            return OCRResult(text="", lines=[], confidence=0.0)

        # Match each cell
        lines = []
        all_confidences = []
        text_margins = []
        cells = []

        for row in range(rows):
            line_chars = []
            row_cells = []
            for col in range(cols):
                y = row * cell_h
                x = col * cell_w
                cell = gray[y:y + cell_h, x:x + cell_w]

                # Resize cell to font dimensions if needed
                if cell.shape != (font.height, font.width):
                    cell = self._resize(cell, font.width, font.height)

                # Binarise: threshold at mean
                threshold = cell.mean()
                binary = (cell > threshold).astype(np.float32)
                is_blank = binary.sum() < 0.05 * binary.size

                # Match against all glyphs
                best_code, best_conf, margin = self._match_glyph(binary, font)
                char = _CP437_MAP.get(best_code, chr(best_code) if best_code < 128 else "?")
                line_chars.append(char)
                all_confidences.append(best_conf)
                if not is_blank:
                    text_margins.append(margin)
                row_cells.append({
                    "char": char, "code": best_code,
                    "conf": round(best_conf, 3), "margin": round(margin, 3),
                })

            lines.append("".join(line_chars).rstrip())
            cells.append(row_cells)

        # Strip trailing empty lines
        while lines and not lines[-1].strip():
            lines.pop()

        text = "\n".join(lines)
        avg_conf = sum(all_confidences) / max(len(all_confidences), 1)
        avg_margin = sum(text_margins) / len(text_margins) if text_margins else 0.0

        result = OCRResult(
            text=text,
            lines=lines,
            grid_width=cols,
            grid_height=rows,
            cell_width=cell_w,
            cell_height=cell_h,
            confidence=avg_conf,
            cells=cells,
            match_margin=avg_margin,
            font_native=getattr(font, "is_native", True),
        )
        self._last_result = result
        return result

    @property
    def last_result(self) -> OCRResult | None:
        return self._last_result

    def export_font_json(self) -> dict:
        """Export loaded font glyphs for canvas bitmap rendering.

        Each glyph row is encoded as an integer bitmask (MSB = leftmost pixel).
        Returns {name, cell_width, cell_height, glyphs: {<code>: [row_int, ...]}}.
        """
        if not self._fonts:
            return {"name": "none", "cell_width": 8, "cell_height": 16, "glyphs": {}}
        font = self._fonts[0]
        glyphs: dict[str, list[int]] = {}
        for code, glyph in font.glyphs.items():
            rows = []
            for r in range(font.height):
                row_bits = 0
                for c in range(font.width):
                    if r < glyph.shape[0] and c < glyph.shape[1]:
                        if glyph[r, c] > 0.5:
                            row_bits |= (1 << (font.width - 1 - c))
                rows.append(row_bits)
            glyphs[str(code)] = rows
        return {
            "name": font.name,
            "cell_width": font.width,
            "cell_height": font.height,
            "glyphs": glyphs,
        }

    def _detect_cell_size(self, gray: np.ndarray, font: FontTemplate) -> tuple[int, int]:
        """Detect character cell size from the image.

        A terminal font must tile the frame: the previous heuristic scored
        only edge energy at candidate boundaries, which preferred large cells
        (e.g. 12x24 on a 1280x800 console) because they crossed more glyph
        strokes.  That wrong stride shifted every row/column and produced
        garbage text.  Here each candidate is scored by actually matching a
        sample of its cells against the font, with a penalty for a grid that
        does not tile the frame.
        """
        h, w = gray.shape

        # Try common cell sizes (font native first so ties win).
        candidates = [
            (font.width, font.height),
            (8, 16), (9, 16), (8, 14), (8, 8),
            (10, 18), (10, 20), (12, 24),  # larger terminal fonts
            (7, 14), (6, 12),              # smaller fonts
            (16, 32), (16, 16),            # doubled / scaled consoles
        ]

        # Pre-normalise glyph templates once for vectorised cell matching.
        codes = list(font.glyphs.keys())
        if not codes:
            return (font.width, font.height)
        glyph_matrix = np.stack(
            [font.glyphs[c].reshape(-1) for c in codes]
        ).astype(np.float32)
        glyph_norms = np.linalg.norm(glyph_matrix, axis=1)

        best_size = (font.width, font.height)
        best_score = -1.0

        for cw, ch in candidates:
            if cw > w or ch > h:
                continue
            cols = w // cw
            rows = h // ch
            if cols < 10 or rows < 5:
                continue

            # Sample a bounded subset of cells: enough to measure glyph
            # agreement without an O(frame) cost per candidate.
            row_step = max(1, rows // 10)
            col_step = max(1, cols // 20)
            corrs: list[float] = []
            margins: list[float] = []

            for row in range(0, rows, row_step):
                for col in range(0, cols, col_step):
                    cell = gray[row * ch:row * ch + ch, col * cw:col * cw + cw]
                    if cell.shape != (font.height, font.width):
                        cell = self._resize(cell, font.width, font.height)
                    binary = (cell > cell.mean()).astype(np.float32)
                    if binary.sum() < 0.05 * binary.size:
                        continue  # blank cell, uninformative
                    vec = binary.reshape(-1)
                    norm = float(np.linalg.norm(vec))
                    if norm < 1e-9:
                        continue
                    sims = glyph_matrix @ vec / (glyph_norms * norm + 1e-10)
                    if sims.size >= 2:
                        top2 = np.argpartition(sims, -2)[-2:]
                        pair = sorted((float(sims[i]) for i in top2), reverse=True)
                        corrs.append(pair[0])
                        margins.append(pair[0] - pair[1])
                    else:
                        corrs.append(float(sims.max()))
                        margins.append(float(sims.max()))

            if not corrs:
                continue

            mean_corr = sum(corrs) / len(corrs)
            mean_margin = sum(margins) / len(margins)

            # A real text grid tiles the frame.  Non-tiling candidates drift
            # across glyphs and must not win on a marginal score.
            rem_w = w % cw
            rem_h = h % ch
            if rem_w == 0 and rem_h == 0:
                tile_score = 1.0
            elif rem_w + rem_h <= 2:
                tile_score = 0.85
            else:
                tile_score = 0.6

            # Reward both a good match and a clear margin over the runner-up.
            score = mean_corr * (1.0 + min(1.0, mean_margin / 0.15)) * tile_score

            if score > best_score:
                best_score = score
                best_size = (cw, ch)

        return best_size

    def _match_glyph(self, cell: np.ndarray, font: FontTemplate) -> tuple[int, float, float]:
        """Find the best matching glyph for a cell.

        Returns (code, correlation, margin) where margin is the correlation
        gap to the second-best glyph.  Raw correlation alone is not enough:
        arbitrary pixels can correlate well with *some* glyph, but they match
        many glyphs almost equally well, so the margin stays small.
        """
        best_code = 32  # space
        best_corr = -1.0
        second_corr = -1.0

        # Quick check: if cell is nearly empty, it's a space
        if cell.sum() < 0.05 * cell.size:
            return (32, 0.95, 1.0)

        for code, glyph in font.glyphs.items():
            # Normalised correlation
            corr = np.sum(cell * glyph) / (
                max(np.sqrt(np.sum(cell ** 2) * np.sum(glyph ** 2)), 1e-10)
            )
            if corr > best_corr:
                second_corr = best_corr
                best_corr = corr
                best_code = code
            elif corr > second_corr:
                second_corr = corr

        margin = max(0.0, best_corr - second_corr) if second_corr > -1.0 else max(0.0, best_corr)
        return (best_code, float(best_corr), float(margin))

    @staticmethod
    def _resize(arr: np.ndarray, target_w: int, target_h: int) -> np.ndarray:
        """Nearest-neighbour resize of a 2D array."""
        h, w = arr.shape
        y_idx = (np.arange(target_h) * h // target_h).astype(int)
        x_idx = (np.arange(target_w) * w // target_w).astype(int)
        return arr[np.ix_(y_idx, x_idx)]
