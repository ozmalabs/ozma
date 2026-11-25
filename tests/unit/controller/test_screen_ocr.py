"""Tests for screen-read OCR selection, confidence and bitmap geometry.

Regression coverage for the live VNC validation where ``screen/read`` returned
27 garbage regions from the 1280x800 VGA console: Tesseract's TSV confidence
parser discarded every word (float string -> int() ValueError) and the bitmap
matcher accepted a wrong 12x24 grid as high confidence.
"""

import asyncio
import shutil

import pytest

pytestmark = pytest.mark.unit


# ── Helpers ────────────────────────────────────────────────────────────────

def _reader(*, tesseract: bool, text_capture=None):
    """Build a ScreenReader without loading fonts / vision providers."""
    from screen_reader import ScreenReader
    reader = ScreenReader.__new__(ScreenReader)
    reader._tesseract_available = tesseract
    reader._text_capture = text_capture
    reader._vision = None
    return reader


def _fake_bitmap_result(*, native: bool = True, margin: float = 0.9,
                        confidence: float = 0.95, text: str = "HELLO WORLD"):
    from text_capture import OCRResult
    cells = [[
        {"char": c, "code": ord(c), "conf": 0.95, "margin": margin}
        for c in text
    ]]
    return OCRResult(
        text=text,
        lines=[text],
        grid_width=len(text),
        grid_height=1,
        cell_width=8,
        cell_height=16,
        confidence=confidence,
        cells=cells,
        match_margin=margin,
        font_native=native,
    )


class _FakeTextCapture:
    def __init__(self, result):
        self._result = result

    def recognise_frame(self, _arr):
        return self._result


def _render_vga_lines(lines, *, width=1280, height=800):
    """Render ``(text, row, col)`` tuples with a real console bitmap font.

    Returns ``(rgb_array, font)`` or ``(None, None)`` when no native PSF font
    is installed on the host.
    """
    numpy = pytest.importorskip("numpy")
    from text_capture import TextCapture

    tc = TextCapture()
    if not tc._fonts or not tc._fonts[0].is_native:
        return None, None
    font = tc._fonts[0]

    arr = numpy.zeros((height, width, 3), dtype=numpy.uint8)
    for text, row, col in lines:
        for i, ch in enumerate(text):
            glyph = font.glyphs.get(ord(ch), font.glyphs.get(32))
            if glyph is None:
                continue
            y = row * font.height
            x = (col + i) * font.width
            if y + font.height > height or x + font.width > width:
                continue
            arr[y:y + font.height, x:x + font.width] = (
                (glyph * 255).astype(numpy.uint8)[:, :, None]
            )
    return arr, font


# ── Tesseract path ─────────────────────────────────────────────────────────

def test_tesseract_tsv_float_confidence_is_parsed():
    """Tesseract 5 emits ``"91.811768"``; int() used to abort the whole pass."""
    from screen_reader import _parse_confidence

    assert _parse_confidence("91.811768") == pytest.approx(0.91811768)
    assert _parse_confidence("80") == pytest.approx(0.80)
    assert _parse_confidence("-1") == 0.0
    assert _parse_confidence(None) == 0.0


def test_tesseract_reads_known_text_when_binary_present():
    if not shutil.which("tesseract"):
        pytest.skip("tesseract binary not installed")

    image_mod = pytest.importorskip("PIL.Image")
    draw_mod = pytest.importorskip("PIL.ImageDraw")
    font_mod = pytest.importorskip("PIL.ImageFont")

    img = image_mod.new("RGB", (1000, 300), "black")
    draw = draw_mod.Draw(img)
    try:
        font = font_mod.truetype(
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 48)
    except OSError:
        font = font_mod.load_default()
    draw.text((30, 80), "HELLO WORLD 123", fill="white", font=font)

    reader = _reader(tesseract=True, text_capture=None)
    state = asyncio.run(reader.read_screen(img, level="tesseract"))

    assert state.ocr_method == "tesseract"
    assert state.ocr_status == "ok"
    assert state.ocr_confidence > 0.5
    assert "HELLO" in state.raw_text.upper()
    assert "WORLD" in state.raw_text.upper()
    assert "123" in state.raw_text


def test_large_terminal_frame_prefers_tesseract_over_bitmap():
    """A 1280x800 frame must not be handed to the bitmap matcher first."""
    image_mod = pytest.importorskip("PIL.Image")
    img = image_mod.new("RGB", (1280, 800), "black")

    reader = _reader(tesseract=True, text_capture=None)

    async def fake_advanced(_img, _arr, _screen_type):
        from screen_reader import TextRegion
        return [TextRegion("TESSERACT WINS", 10, 20, 160, 16, 0.95)]

    async def fail_bitmap(_arr, _state):
        raise AssertionError("bitmap OCR must not run before Tesseract on large frames")

    reader._advanced_ocr = fake_advanced
    reader._bitmap_ocr_scored = fail_bitmap

    state = asyncio.run(reader.read_screen(img, level="auto"))

    assert state.ocr_method == "tesseract"
    assert state.ocr_status == "ok"
    assert state.raw_text == "TESSERACT WINS"


# ── Bitmap fallback / confidence ───────────────────────────────────────────

def test_bitmap_approximation_is_never_high_confidence():
    image_mod = pytest.importorskip("PIL.Image")
    result = _fake_bitmap_result(native=False)
    reader = _reader(tesseract=False, text_capture=_FakeTextCapture(result))

    state = asyncio.run(reader.read_screen(
        image_mod.new("RGB", (640, 400), "black"), level="auto",
    ))

    assert state.ocr_method == "bitmap"
    assert state.ocr_status == "low_confidence"
    assert state.ocr_confidence < 0.6
    assert state.text_regions  # fallback text is surfaced, but clearly marked
    assert all(r.confidence <= 0.35 for r in state.text_regions)


def test_bitmap_low_glyph_margin_is_low_confidence():
    image_mod = pytest.importorskip("PIL.Image")
    result = _fake_bitmap_result(native=True, margin=0.01)
    reader = _reader(tesseract=False, text_capture=_FakeTextCapture(result))

    state = asyncio.run(reader.read_screen(
        image_mod.new("RGB", (640, 400), "black"), level="auto",
    ))

    assert state.ocr_status == "low_confidence"
    assert state.ocr_confidence < 0.6


def test_no_fabricated_text_when_ocr_unavailable():
    image_mod = pytest.importorskip("PIL.Image")
    reader = _reader(tesseract=False, text_capture=None)

    state = asyncio.run(reader.read_screen(
        image_mod.new("RGB", (1280, 800), "black"), level="auto",
    ))

    assert state.text_regions == []
    assert state.raw_text == ""
    assert state.ocr_method == "unavailable"
    assert state.ocr_status == "unavailable"
    assert state.ocr_confidence == 0.0


def test_bitmap_unavailable_explicit_request_returns_no_text():
    image_mod = pytest.importorskip("PIL.Image")
    reader = _reader(tesseract=False, text_capture=None)

    state = asyncio.run(reader.read_screen(
        image_mod.new("RGB", (640, 400), "black"), level="bitmap",
    ))

    assert state.text_regions == []
    assert state.raw_text == ""
    assert state.ocr_status == "unavailable"


# ── Grid / region geometry ─────────────────────────────────────────────────

def test_detects_native_8x16_grid_on_1280x800_not_wrong_stride():
    """The old edge-energy heuristic picked 12x24 and shifted every glyph."""
    arr, font = _render_vga_lines([
        ("HELLO", 3, 2),
        ("WORLD", 5, 0),
        ("OZMA 1280x800", 10, 0),
    ])
    if arr is None:
        pytest.skip("no native console font installed")

    from text_capture import TextCapture
    tc = TextCapture()
    result = tc.recognise_frame(arr)

    assert (result.cell_width, result.cell_height) == (
        font.width, font.height,
    )
    assert result.grid_width == 1280 // font.width
    assert result.grid_height == 800 // font.height


def test_bitmap_regions_keep_1280x800_geometry():
    arr, font = _render_vga_lines([
        ("HELLO", 3, 2),
        ("WORLD", 5, 0),
        ("OZMA 1280x800", 10, 0),
    ])
    if arr is None:
        pytest.skip("no native console font installed")

    from text_capture import TextCapture
    reader = _reader(tesseract=False, text_capture=TextCapture())
    regions, _confidence, status = reader._bitmap_ocr_scored(arr, None)

    assert status == "ok"
    by_text = {r.text: r for r in regions}
    assert "HELLO" in by_text
    hello = by_text["HELLO"]
    assert (hello.x, hello.y) == (2 * font.width, 3 * font.height)
    assert hello.width == 5 * font.width
    assert hello.height == font.height

    world = by_text["WORLD"]
    assert (world.x, world.y) == (0, 5 * font.height)

    for region in regions:
        assert 0 <= region.x < 1280
        assert 0 <= region.y < 800
        assert region.x + region.width <= 1280
        assert region.y + region.height <= 800


def test_screen_vectors_dimensions_unchanged_at_1280x800():
    numpy = pytest.importorskip("numpy")
    image_mod = pytest.importorskip("PIL.Image")
    reader = _reader(tesseract=False, text_capture=None)

    img = image_mod.fromarray(
        numpy.zeros((800, 1280, 3), dtype=numpy.uint8)
    )
    result = reader.extract_vectors(img)

    assert result["width"] == 1280
    assert result["height"] == 800
    assert result["rectangles"] == []
    assert result["lines"] == []
