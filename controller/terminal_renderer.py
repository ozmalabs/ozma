# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Terminal renderer — display content as ANSI/Unicode art in any VT100 terminal.

Backend priority:
  1. chafa    — best quality; adapts to sixel / Kitty / half-block / truecolor
               automatically based on the target terminal.  pip install chafa.py
  2. half-block — pure-Python fallback using Unicode ▀ characters.  Works in
               any truecolor terminal with no extra dependencies.

Two rendering strategies:

  ocr     Exact text + VGA colours via bitmap OCR (TextCapture).
          Perfect fidelity for text-mode displays: BIOS, console, grub, DOS.
          No downsampling — one terminal cell per character cell.

  pixel   Convert the display frame to pixel art via chafa (or half-block).
          Works for any content including graphical UIs.
          chafa auto-picks the best protocol the terminal supports.

Auto mode: run OCR on the frame; if confidence ≥ 0.60 use ocr, else pixel.

Usage from the terminal (via HTTP streaming endpoint or term_view.py CLI):
  curl -s http://localhost:7380/api/v1/remote/vm1/view                 # snapshot
  curl -s http://localhost:7380/api/v1/remote/vm1/view?stream=1        # live
  python3 controller/term_view.py vm1                                  # CLI
"""

from __future__ import annotations

import asyncio
import io
import logging
import time
from typing import AsyncIterator, Callable, Awaitable

log = logging.getLogger("ozma.terminal_renderer")

try:
    import numpy as np
    from PIL import Image as _PILImage
    _DEPS = True
except ImportError:
    _DEPS = False
    log.debug("PIL/numpy not available — terminal_renderer pixel mode disabled")

try:
    import chafa
    _CHAFA = True
except ImportError:
    _CHAFA = False


# ── Chafa backend ─────────────────────────────────────────────────────────────

def _chafa_render(
    img: "_PILImage.Image",
    cols: int,
    rows: int,
    pixel_mode: str = "auto",
) -> bytes:
    """
    Render a PIL Image via chafa.

    pixel_mode: "auto" | "sixel" | "kitty" | "symbols" | "half"
    Returns ANSI/sixel bytes ready to write to a terminal.
    """
    canvas_config = chafa.CanvasConfig()
    canvas_config.width = cols
    canvas_config.height = rows

    # chafa.PixelMode choices: SIXEL, KITTY, SYMBOLS, BRAILLE, HALF_BLOCKS, etc.
    match pixel_mode:
        case "sixel":
            canvas_config.pixel_mode = chafa.PixelMode.SIXEL
        case "kitty":
            canvas_config.pixel_mode = chafa.PixelMode.KITTY
        case "half":
            canvas_config.pixel_mode = chafa.PixelMode.HALF_BLOCKS
        case "braille":
            canvas_config.pixel_mode = chafa.PixelMode.BRAILLE
        case _:
            # Let chafa decide — it queries TERM/COLORTERM env for capabilities
            pass  # default is SYMBOLS with truecolor fallback

    canvas_config.color_mode = chafa.ColorMode.TRUECOLOR

    canvas = chafa.Canvas(canvas_config)
    w, h = img.size
    rgba = img.convert("RGBA")
    pixels = rgba.tobytes()
    canvas.draw_all_pixels(chafa.PixelType.RGBA8_PREMULTIPLIED, pixels, w, h, w * 4)
    return canvas.print().encode("utf-8")


# ── Pure-Python half-block fallback ──────────────────────────────────────────

def _halfblock_render(img: "_PILImage.Image", cols: int, rows: int) -> bytes:
    """
    Pure-Python half-block renderer (▀ characters).
    No external dependencies beyond PIL + numpy.
    Each terminal character = 1 pixel wide × 2 pixels tall.
    """
    src_w, src_h = img.size
    scale = min(cols / src_w, (rows * 2) / src_h)
    out_w = max(1, int(src_w * scale))
    out_h = max(2, int(src_h * scale))
    out_h += out_h % 2

    img = img.resize((out_w, out_h), _PILImage.LANCZOS)
    arr = np.array(img.convert("RGB"), dtype=np.uint8)

    out: list[str] = []
    for row in range(0, out_h, 2):
        cur_fg = cur_bg = None
        for col in range(out_w):
            top = (int(arr[row, col, 0]), int(arr[row, col, 1]), int(arr[row, col, 2]))
            bot_row = min(row + 1, out_h - 1)
            bot = (int(arr[bot_row, col, 0]), int(arr[bot_row, col, 1]), int(arr[bot_row, col, 2]))

            if top == bot:
                char, fg, bg = " ", (0, 0, 0), top
            else:
                char, fg, bg = "▀", top, bot

            if fg != cur_fg:
                out.append(f"\x1b[38;2;{fg[0]};{fg[1]};{fg[2]}m")
                cur_fg = fg
            if bg != cur_bg:
                out.append(f"\x1b[48;2;{bg[0]};{bg[1]};{bg[2]}m")
                cur_bg = bg
            out.append(char)

        out.append("\x1b[0m\r\n")

    return "".join(out).encode("utf-8")


# ── OCR text renderer ─────────────────────────────────────────────────────────

def _ocr_render(img: "_PILImage.Image") -> bytes:
    """
    Render via VGA bitmap OCR — exact text + VGA colors.
    Returns full ANSI repaint bytes.
    """
    from text_capture import TextCapture
    from terminal_bridge import _build_cell_grid, full_repaint

    tc = TextCapture()
    arr = np.array(img.convert("RGB"), dtype=np.uint8)
    result = tc.recognise_frame(img)
    if not result.lines:
        return b""
    grid = _build_cell_grid(arr, result, tc._fonts)
    return full_repaint(grid)


# ── Public API ────────────────────────────────────────────────────────────────

def render_frame(
    jpeg: bytes,
    mode: str = "auto",
    cols: int = 80,
    rows: int = 24,
    home: bool = False,
    pixel_mode: str = "auto",
) -> bytes:
    """
    Render a JPEG frame as ANSI terminal art.

    Args:
        jpeg:       JPEG bytes from a snapshot endpoint
        mode:       "auto" | "ocr" | "pixel"
        cols/rows:  target terminal dimensions
        home:       True = cursor-home (no clear; for in-place streaming updates)
        pixel_mode: chafa pixel mode hint: "auto"|"sixel"|"kitty"|"half"|"braille"

    Returns bytes to write directly to a terminal.
    """
    if not _DEPS:
        return b"[terminal_renderer: PIL/numpy not installed]\r\n"

    img = _PILImage.open(io.BytesIO(jpeg)).convert("RGB")

    prefix = b"\x1b[H" if home else b"\x1b[2J\x1b[H"

    if mode == "ocr":
        return prefix + _ocr_render(img)

    if mode == "pixel":
        return prefix + _pixel_render(img, cols, rows, pixel_mode)

    # auto: try OCR first
    from text_capture import TextCapture
    result = TextCapture().recognise_frame(img)
    if result.confidence >= 0.60 and result.lines:
        return prefix + _ocr_render(img)
    return prefix + _pixel_render(img, cols, rows, pixel_mode)


def _pixel_render(img: "_PILImage.Image", cols: int, rows: int, pixel_mode: str) -> bytes:
    if _CHAFA:
        try:
            return _chafa_render(img, cols, rows, pixel_mode)
        except Exception as e:
            log.debug("chafa render failed, falling back to half-block: %s", e)
    return _halfblock_render(img, cols, rows)


def backend_name() -> str:
    """Return the name of the active pixel rendering backend."""
    return "chafa" if _CHAFA else "half-block (pure Python)"


# ── Async streaming generator ─────────────────────────────────────────────────

async def stream_frames(
    frame_fn: Callable[[], Awaitable[bytes | None]],
    mode: str = "auto",
    fps: float = 10.0,
    cols: int = 80,
    rows: int = 24,
    pixel_mode: str = "auto",
) -> AsyncIterator[bytes]:
    """
    Async generator: captures frames and yields ANSI bytes at up to `fps`.

    First frame uses clear-screen; subsequent frames use cursor-home for
    in-place updates without terminal flicker.
    """
    interval = 1.0 / max(fps, 0.1)
    first = True
    loop = asyncio.get_event_loop()

    while True:
        t0 = time.monotonic()
        try:
            jpeg = await asyncio.wait_for(frame_fn(), timeout=3.0)
        except (asyncio.TimeoutError, Exception):
            await asyncio.sleep(interval)
            continue

        if jpeg is None:
            await asyncio.sleep(interval)
            continue

        try:
            data = await loop.run_in_executor(
                None,
                lambda j=jpeg, h=not first: render_frame(j, mode, cols, rows, h, pixel_mode),
            )
            if data:
                yield data
            first = False
        except Exception as e:
            log.debug("stream_frames: render error: %s", e)

        await asyncio.sleep(max(0.0, interval - (time.monotonic() - t0)))
