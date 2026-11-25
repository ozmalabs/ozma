# Authoring widgets and themes

This guide is for authors of Ozma display widgets, theme packs, and design
metadata. It describes the intended authoring model; check the milestone table
in the [execution plan](../display-platform-execution-plan.md) before relying on
a renderer feature in production. Renderer adapters are still being completed,
so a contract can exist before every target consumes it.

## Widget IR

`WidgetIR` is the lossless interchange model for a widget. A widget has:

- an identity and version (`id`, `version`, `schema`);
- source provenance, including the original format and raw document;
- a tree of nodes with geometry, style, data bindings, and children;
- asset references (images, fonts, scripts) that are content-addressed;
- design-language metadata (`vibe`, `shape`, `coherence`);
- translation metadata used to build export reports.

Author original IR when you can. The contracts and validators are implemented
in `ozma-display-ir` (Rust), `controller/display_ir.py` (Python), and the JSON
Schemas in `schemas/display/`. Existing packs and `DisplayDefinition` wire
payloads remain compatible; full IR is stored with the pack rather than
inflated into the wire definition.

Practical rules:

1. Use explicit units and a clear origin. Do not rely on a particular renderer's
   default coordinate system.
2. Keep bindings declarative. A binding names a data source and a projection;
   it must not embed a script or a renderer-specific expression.
3. Preserve unknown fields on read. If your tool does not understand an
   extension, round-trip it untouched.
4. Reference assets by pack-relative path and SHA-256. Absolute paths and
   network URLs are not portable.
5. Keep the wire projection small. The wire definition is limited to 1 MiB;
   full source, previews, and large assets belong in the pack.

## Theme tokens

Themes express a design language through tokens rather than per-widget colours.
The built-in `core-dark` theme is the reference implementation. A theme should
define:

- **Palette**: surface, elevated surface, text, muted text, accent, positive,
  warning, critical, and focus colours. Provide contrast-safe defaults.
- **Typography**: role-based families and sizes (display, heading, body,
  label, numeric, monospace) plus weight and letter spacing.
- **Spacing and radius**: a small, named scale; avoid arbitrary per-widget
  values.
- **Motion**: duration and easing tokens, with a reduced-motion variant.
- **Density**: compact and comfortable if the target supports both.
- **Shape**: corner radii, stroke weights, and gauge/arc vocabulary.

Widgets should consume tokens and expose only intentional overrides. A pack
that hard-codes a complete visual style is harder to make coherent and harder
to translate to constrained targets.

## Vibe, shape, and coherence

Design language is captured, not inferred away.

- **Source tags** come from the imported document or pack author.
- **User tags** come from an explicit user choice and always win.
- **Derived tags** are produced by extraction and carry `provenance` and
  `confidence`.

The versioned taxonomy is `org.ozma.vibe/1`. Unknown keywords are retained even
when the current taxonomy does not recognise them. Shape signatures summarise
geometry and visual vocabulary (for example arc, bar, card, or needle). The
coherence report is advisory: it can point out conflicts or outliers, but it
must not rewrite source or user intent.

Authoring guidance:

- Describe the intended mood in plain language and add stable keywords.
- Do not fabricate confidence. If a tag is a guess, mark it derived and give a
  realistic confidence.
- When intentionally mixing styles, record that decision as a user override
  instead of deleting the conflicting source tags.
- Keep previews representative; a preview that hides the dominant shape makes
  repository filtering and coherence checks unreliable.

## Accessibility

Accessibility is part of the contract, not a post-processing step.

- Meet contrast targets for text and meaningful graphics; do not rely on colour
  alone to convey state.
- Provide text equivalents for gauges, sparklines, and status tiles. A value,
  unit, and state should be readable without seeing the graphic.
- Respect reduced-motion settings. Needle sweeps, glows, and transitions must
  have a static or shortened variant.
- Use target sizes appropriate to touch panels, and keep focus indicators
  visible.
- Support text scaling without clipping labels or overlapping bindings.
- Avoid flashing content and unbounded animation.
- Mark decorative assets as decorative so assistive technology can skip them.

The browser adapter already applies accessibility and reduced-motion behaviour
to the implemented gauge/bar family. Other targets are expected to follow the
same semantics as they are completed.

## Size budgets

Budgets keep installs, sync, and constrained renderers predictable.

| Item | Budget | Notes |
|---|---|---|
| Wire definition | 1 MiB | Full IR/source stays in the pack |
| Signed index | 2 MiB gzip | Target for 10k packs |
| Pack install/update | 10 MiB in 5 s | Controller-class hardware |
| Browser first render | 250 ms | After the definition is available |
| Controller PNG frame | 100 ms | 1280×720 reference theme |
| Font subset | 2 s | 300-glyph Latin/CJK target |
| LVGL font generation | 5 s | Per theme |
| Default pack payload | 64 MiB | `OZMA_DISPLAY_MAX_PACK_BYTES` |
| Default import payload | 32 MiB | `OZMA_DISPLAY_MAX_IMPORT_BYTES` |

The repository also enforces depth, file-count, per-file, and total-size limits
when reading directories and archives. A pack that exceeds a limit fails
closed rather than partially installing.

## Authoring checklist

- [ ] IR validates against the current JSON Schema and Python/Rust validators.
- [ ] Assets are relative, hashed, and declared in the manifest.
- [ ] Executable assets declare an iframe/WASM sandbox.
- [ ] Theme tokens cover palette, typography, spacing, motion, and focus.
- [ ] Vibe/shape metadata is separated into source, user, and derived tags.
- [ ] Accessibility text, contrast, reduced motion, and scaling are checked.
- [ ] The pack stays within size budgets.
- [ ] `python scripts/check_display_licenses.py --root . --mode local` passes.
- [ ] The licence is on the published allowlist if the pack will be submitted.

See the [research document](../display-widget-library-research.md) for the
rationale behind the IR, vibe/shape, and theme choices.
