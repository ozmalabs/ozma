# Fonts and typography

Font translation matches a requested font role and shape against available
fonts, then reports every substitution and every export decision. It does not
try to clone a proprietary family name.

Font translation is behind `OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED` (default
`false`). Both `ozma-fonts` and `controller/font_translation.py` provide TTF/OTF
metadata extraction, PANOSE and metric analysis, `fsType` licence checks,
deterministic shape/intent matching, and export planning.
`controller/font_export.py` performs TTF/OTF subsetting, BDF and LVGL C export,
and PIL previews. WOFF2 writing needs the optional `brotli` package and fails
closed when it is absent.

## Font intent

A `FontIntent` describes what a font is for, not just what family was named:

- **Role**: display, heading, body, label, numeric, monospace, icon, or
  decorative.
- **Classification**: serif, sans, slab, rounded, condensed, script, bitmap, and
  similar shape vocabulary.
- **Metrics**: target size, weight, width, x-height expectation, and tabular
  number needs.
- **Coverage**: scripts, glyph ranges, symbols, ligatures, and shaping features.
- **Axes**: variable-font axes such as weight, width, optical size, or slant.
- **Constraints**: device profile (browser, PNG, LVGL, AIDA64, Adaptive Cards),
  available disk/flash, and bitmap depth.

The intent is stored with the theme or widget. It survives import even when the
original font cannot be embedded.

## Shape and intent matching

Matching is deterministic and explainable. Candidate fonts are scored on:

1. role fit;
2. classification and shape similarity;
3. metric compatibility (size, width, weight, x-height);
4. required coverage, with missing glyphs treated as a hard or heavily
   penalised failure;
5. target-specific constraints such as bitmap depth or flash size;
6. licence suitability for the destination.

Ties break deterministically, and the ordered candidate list is stable for the
same corpus. A user override wins over the scored result. The matcher records
why it chose a font and why alternatives were rejected so the font report is
useful rather than opaque.

## Embedding licences

Embedding is a separate right from using a font to render a preview.

- `fsType` is authoritative for installable/editable/embeddable permissions.
  Fonts that forbid embedding are substituted, never embedded.
- OFL-1.1 fonts are acceptable, but an OFL font with a Reserved Font Name (RFN)
  must not be redistributed under the reserved name after modification. Keep
  the original name for unmodified subsets, or rename modified versions
  according to the licence.
- The published allowlist includes OFL-1.1, MIT, BSD-2-Clause, BSD-3-Clause,
  ISC, Apache-2.0, and CC0-1.0.
- Do not vendor fonts from proprietary operating systems, AIDA64/Rainmeter
  skins, or other sources without explicit redistribution rights.
- A permissive font licence does not override a separate restriction on a
  bundled icon set or artwork.

The `fsType` gate is implemented in `ozma-fonts`. A non-embeddable font may
still be used for local rendering if the terms allow it; the export path must
substitute a licensed font and record the decision.

## Substitution reports

Every font change emits a `FontTranslationReport` attached to the export. It
records, at minimum:

- requested intent and source font;
- selected substitute and its licence;
- substitutions and why they happened;
- dropped glyphs/features and approximated metrics or spacing;
- rasterisation or subsetting decisions;
- target profile and any fallback chain;
- user overrides and their scope.

A report must make loss visible. Silent fallback to a default font is a bug.

## Export targets

| Target | Output | Current state |
|---|---|---|
| Browser | WOFF2/TTF/OTF subset, CSS font-face | Planned; metadata/matching foundation exists |
| Controller PNG | Rasterised faces via the renderer | Partial; renderer work tracked separately |
| LVGL | C font arrays at selectable bpp | Planned |
| u8g2 / embedded | BDF/bitmap or converted font data | Planned |
| AIDA64 HTML/SSE | CSS/webfont references in the generated panel | Planned with the AIDA64 adapter |
| Adaptive Cards | Host-supported font roles and fallbacks | Planned |

Subsetting must preserve required tables, glyph coverage, shaping behaviour, and
licence obligations. A subset that changes advance widths or breaks shaping is
not a lossless-enough export; record it as an approximation in the report.

## Operational checks

- Validate `fsType` and OFL RFN before publishing a pack containing fonts.
- Prefer original, OFL-licensed themes and fonts for shipped packs.
- Keep font files content-addressed by SHA-256 in the asset cache.
- Bound subsetting and generation time (see the performance budgets in the
  execution plan: 2 s per 300-glyph subset, 5 s per LVGL theme font).
- Include the font licence and notices in the pack.

See the [research document](../display-widget-library-research.md) for the font
intent model and licensing analysis, and the
[execution plan](../display-platform-execution-plan.md) §7 (M4) for the font
task backlog.
