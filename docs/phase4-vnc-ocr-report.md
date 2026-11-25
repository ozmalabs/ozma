# Phase 4 — VNC screen-read OCR fix (2026-11-25)

Scope: controller `screen/read` OCR selection and the bitmap font decoder
(`controller/screen_reader.py`, `controller/text_capture.py`) plus tests.
No Rust/node, VNC transport, or `/screen/vectors` behaviour was changed.
No commit was made.

## Symptom from the live VNC validation

`POST /api/v1/screen/read` returned HTTP 200 with the correct 1280x800 frame
geometry and 27 text regions, but `ocr_method=bitmap` produced garbage such as
`*Nk*Wf9MUH**'H+ rHpwmmmw[[uNq#>@m@m[qg#gmpu6q@&mHKmw`. Tesseract applied to
the same saved frame read the Alpine boot text and `localhost login:` line
correctly.

The saved validation frame is still available at
`/tmp/opencode/webrtc-reval-artifacts/ctrl-snapshot-vm1.jpg` (1280x800) and was
used to reproduce and verify the fix.

## Root causes

There were four independent defects; any one of them could return bad output.

1. **Tesseract CLI results were silently discarded.**
   `_basic_ocr_pass()` parsed the TSV confidence column with `int()`. Tesseract
   5 emits a float string (`"91.811768"`), so `int()` raised `ValueError`; the
   surrounding `except Exception: return []` swallowed it and every Tesseract
   word was lost. Measured before the fix: `_advanced_ocr()` on the saved frame
   returned **0 regions**, even though the `tesseract` binary read the frame
   correctly. This defeated the intended Tesseract fallback.

2. **The font loader missed the installed VGA fonts.**
   `TextCapture._load_system_fonts()` only looked in `/usr/share/consolefonts`
   and `/usr/lib/kbd/consolefonts` for `*.psf` names. This host (like Arch/
   Fedora and the `kbd` package) stores fonts under
   `/usr/share/kbd/consolefonts`, often as `.psfu.gz` (for example
   `default8x16.psfu.gz`). The loader therefore fell back to an anti-aliased
   Pillow/DejaVu approximation, which cannot match real VGA console glyphs.

3. **The cell-size detector chose a wrong row/column stride.**
   `_detect_cell_size()` scored candidates only by edge energy at candidate
   boundaries, without validating that the glyphs actually matched or that the
   grid tiled the frame. On the 1280x800 console it picked **12x24** instead of
   the real **8x16** grid. Every row/column was then offset, which is the
   scaling/stride bug behind the garbled transcript. A plain RGB channel mean
   also shifted the grayscale enough to make the detector pick a half-size
   **4x8** grid on the same frame.

4. **Bitmap confidence was not discriminative.**
   The bitmap path accepted output when the average best-glyph cosine
   correlation exceeded 0.6. Arbitrary GUI/text pixels always correlate well
   with *some* template, so the garbage output scored **0.841** and was
   presented as a high-confidence `bitmap` transcript. There was no
   second-best margin, no printable/alphanumeric plausibility check, and no
   distinction between a real console font and the Pillow approximation.

## Fix

### `controller/text_capture.py`

- `_load_system_fonts()` now searches `/usr/share/kbd/consolefonts` in
  addition to the existing directories, tries `.psf`, `.psfu`, `.gz`,
  `.psf.gz`, `.psfu.gz`, and `.f16` suffixes, and finally tries any PSF font
  in those directories. `FontTemplate.is_native` distinguishes a real console
  font from the Pillow fallback.
- Bitmap input is converted with Rec. 601 luminance instead of an RGB channel
  mean (alpha is ignored). This keeps coloured/subpixel-antialiased console
  text on the correct grid.
- `_detect_cell_size()` now samples each candidate grid and actually matches
  those cells against the font. A candidate wins only with both a good match
  and a clear margin over the runner-up, and non-tiling grids are penalised.
  On the saved 1280x800 frame this selects **8x16**, not 12x24 or 4x8.
- `_match_glyph()` returns `(code, correlation, margin)`; `OCRResult` carries
  `match_margin` and `font_native` so callers can calibrate trust.

### `controller/screen_reader.py`

- Tesseract confidence parsing accepts `int`, `float`, and numeric strings via
  `_parse_confidence()`. The CLI path now skips malformed rows instead of
  aborting the whole pass, and logs the failure.
- New `_bitmap_ocr_scored()` computes a calibrated confidence from:
  best-glyph correlation, second-best margin, printable/alphanumeric ratio,
  and whether a native console font was used. It returns
  `(regions, confidence, status)` with status `ok`, `low_confidence`, or
  `unavailable`. Region confidence is set to the calibrated value, never the
  raw correlation.
- `read_screen()` now prefers Tesseract when it is installed and the frame is
  large (>= 800x600) or Tesseract was explicitly requested. The bitmap matcher
  remains the fast path for classic small text modes. If Tesseract was
  preferred but returned nothing, bitmap OCR is tried once as a fallback.
- A weak bitmap result is only surfaced with an explicit
  `ocr_status=low_confidence` marker when no real OCR engine is available.
  When Tesseract is installed but reads nothing, the result is typed
  `ocr_status=unavailable` with empty text/regions rather than fabricated
  text. Explicit `level=bitmap` still returns bitmap output, but never as
  high confidence.
- `ScreenState` now exposes `ocr_method`, `ocr_confidence`, and `ocr_status`
  through `to_dict()` and `to_prompt()`.
- Tesseract regions are sorted top-to-bottom/left-to-right before `raw_text`
  is composed, so the transcript is readable instead of confidence-ordered.

`/screen/vectors` was not modified: `extract_vectors()` still returns
`width`/`height` plus `rectangles`/`lines`, and its 1280x800 dimensions are
covered by a regression test.

## Evidence against the saved live frame

Command-level check on `/tmp/opencode/webrtc-reval-artifacts/ctrl-snapshot-vm1.jpg`:

| Path | Before | After |
| --- | --- | --- |
| `_advanced_ocr()` | 0 regions (TSV float parse aborted) | 236 regions |
| `read_screen(level="auto")` | `ocr_method=bitmap`, garbage, avg conf 0.841 | `ocr_method=tesseract`, `ocr_status=ok`, conf ≈0.804 |
| Bitmap grid | 12x24 (later 4x8 with RGB mean) | 8x16 |
| Raw text prefix | `*Nk*Wf9MUH**'H+ ...` | `Booting from DUD/CD... ISOLINUX 6.04 ...` |

The corrected `raw_text` contains the boot lines and the
`localhost login:`/`Welcome to Alpine Linux` region; the tesseract transcript
still has a few recognition substitutions (`filesystens`, `harduare`) typical
of the anti-aliased capture, but it is no longer fabricated glyph soup.

Bitmap-only output on the same frame is still imperfect
(`Booting [roo ...`, `ISOLINUX`→`ISOLINUK`), which is exactly why large frames
now prefer Tesseract. On a synthetic 1280x800 frame rendered with the native
8x16 font, bitmap output is exact (`HELLO`, `WORLD`, `OZMA 1280x800`) and the
regions land at the expected cell coordinates.

## Tests

Added `tests/unit/controller/test_screen_ocr.py`:

- Tesseract float-TSV confidence parsing (`"91.811768"` → 0.918).
- Tesseract-present path reads known text (`HELLO WORLD 123`), skipped if the
  binary is absent.
- Large 1280x800 terminal frame prefers Tesseract over bitmap.
- Bitmap fallback is only returned with `ocr_status=low_confidence` and low
  confidence when no real font/Tesseract is available.
- Low glyph margin is low confidence.
- No fabricated text: `ocr_status=unavailable`, empty regions/raw text when
  no OCR engine is available.
- Native 8x16 grid detection on 1280x800 (regression for the 12x24 stride bug).
- Bitmap region geometry at 1280x800 (`x = col*8`, `y = row*16`, no overflow).
- `/screen/vectors` dimensions/empty-rectangle semantics at 1280x800.

Results:

```text
python3 -m pytest tests/unit/controller/test_screen*.py -q
39 passed

python3 -m pytest tests/unit/controller/test_terminal_renderer.py \
  tests/unit/controller/test_screen_server_protobuf.py tests/test_v1_features.py -q
110 passed
```

## Remaining gaps

- **No Tesseract installed:** bitmap output is still only an approximation on
  anti-aliased captures. It is now explicitly marked `low_confidence`; there
  is no high-confidence transcript without a real OCR engine.
- **Bitmap glyph accuracy is still imperfect** on JPEG/anti-aliased VGA
  consoles (e.g. `M`/`N`, `0`/`O`, box-drawing glyphs). The native PSF font and
  correct 8x16 stride make it usable for classic modes, but large/VNC frames
  should continue to use Tesseract.
- **Latency was not benchmarked.** Tesseract on a 1280x800 frame is slower than
  bitmap matching; this fix favours correctness for large frames.
- **`/api/v1/captures/{id}/ocr`** still calls `TextCapture.recognise_frame()`
  directly and reports the legacy correlation in `confidence`. It was outside
  this task's `screen/read` scope, but it does not use the calibrated
  confidence/status signals yet.
- **Live VNC re-validation was not rerun** (the demo VMs/controller were not
  running during this fix). Verification used the saved 1280x800 validation
  frame plus synthetic native-font frames.
