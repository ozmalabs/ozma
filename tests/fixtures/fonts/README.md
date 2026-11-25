# Font fixtures

This directory intentionally contains no font binaries. Font binaries are
large, often have redistribution terms that differ from the Ozma licence, and
are already available as system packages on the platforms that run the
controller and font tests.

## What the tests use

The controller font tests (`tests/unit/controller/test_font_translation.py` and
`tests/unit/controller/test_font_export.py`) resolve fonts from these locations:

| Purpose | Preferred files |
|---|---|
| Sans metadata, PANOSE, coverage, BDF/LVGL raster | `/usr/share/fonts/TTF/DejaVuSans.ttf`, `/usr/share/fonts/dejavu/DejaVuSans.ttf`, `/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf` |
| Serif classification | `DejaVuSerif.ttf` in the same directories |
| Monospace/tabular metrics | `DejaVuSansMono.ttf` in the same directories |
| OFL licence/planning flows | `/usr/share/fonts/liberation/LiberationSans-Regular.ttf`, `/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf` |

If none of the candidates exist, each test calls `pytest.skip()` with a message
that names the missing font family. Install DejaVu or Liberation fonts (for
example `fonts-dejavu-core` and `fonts-liberation` on Debian/Ubuntu) to run the
real export tests.

## Fonts that must not be committed

Do not add DejaVu, Liberation, Segoe UI, SF Pro, Helvetica, Arial, or any other
OS/vendor font, icon font, CJK font, Arabic/Indic font, variable font, or bitmap
font to this directory. They are either not redistributable or too large for a
source tree.

When a future task needs a stable binary fixture, generate a minimal synthetic
font at test time with `fontTools` in `tmp_path` instead. The existing fsType and
`tnum` tests already do this: they copy a system font, change the OS/2 bits or
append a GSUB feature, and discard the copy when the test ends.

## Licence expectations

The tests treat a font's `name` table licence description as untrusted prose.
Prose that happens to contain `All Rights Reserved`, `GPL`, `non-commercial`,
`personal use`, `proprietary`, or `unknown` is rejected even if it also says
"public domain". Pass an explicit SPDX-style licence such as `OFL-1.1` when a
test needs an embeddable asset, and keep imported originals private.

See `docs/display-widget-library-research.md` (Fonts and typography) and
`docs/display-platform-execution-plan.md` (Phase 4) for the policy this corpus
supports.
