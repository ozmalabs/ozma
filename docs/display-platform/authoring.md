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

## Charts and history

The browser adapter renders multi-series charts with the dependency-free
`web/display/widgets/chart.js` module. A `chart` node uses the chart renderer
when it declares series data in one of these forms:

- `metrics`: an array of field keys/ids, with optional `series` entries that
  add labels and colours;
- `series`: an array of metric keys/ids or objects with `metric` (or `field`)
  plus optional `label`, `color`, `unit`, `style`, and `area`;
- `history.series`: the same series descriptors nested under history;
- a history window such as `history: {"window": 90}` or
  `history: {"points": 120}`.

Simple single-series `sparkline` and `history` widgets keep the sparkline
renderer. If `chart.js` is unavailable, a chart falls back to the sparkline or
a readable card. History rings are bounded; the client records every bound
field and updates the existing SVG in place when a field changes.

```json
{
  "type": "chart",
  "label": "Network",
  "metrics": ["@active.RX", "@active.TX"],
  "series": [
    { "label": "RX", "color": "#42d392", "style": "area" },
    { "label": "TX", "color": "#5b6fff", "style": "line" }
  ],
  "min": 0,
  "max": 1000,
  "unit": "Mb/s",
  "warn": 800,
  "crit": 950,
  "baseline": "zero",
  "showLegend": true,
  "showGrid": true,
  "ticks": 5,
  "xTicks": 5,
  "history": { "points": 120 }
}
```

Chart options:

| Option | Meaning |
|---|---|
| `width`, `height` | ViewBox size in user units. Values are clamped to a safe bound. |
| `min`, `max`, `auto` | Explicit or automatic y-range; out-of-range samples clamp visually. |
| `warn`, `crit`, `thresholds`, `bands` | Warning/critical lines and optional severity bands. Bands may be `{from,to,severity,color,opacity}` objects. |
| `style` | `line`, `step`, `area`, or `area-step`; per-series `style` overrides it. |
| `area`, `fill`, `areaOpacity` | Area fill for the whole chart or a series. |
| `baseline` | `zero`, `min`, `max`, or a numeric baseline. |
| `timeWindow` / `windowMs` | Rolling x-axis window in milliseconds when samples carry timestamps. |
| `history.points` / `history.window` | Bounded sample capacity for the per-series ring. |
| `ticks` / `xTicks` | Bounded y/x tick counts. |
| `showAxes`, `showGrid`, `showLegend`, `showLast`, `showValue` | Axis, grid, legend, marker, and value-label toggles. |
| `tooltip` | Set `false` to disable the pointer tooltip. |
| `colors` / `palette` | Explicit colours or a series palette; invalid colours fall back safely. |

Charts follow the same accessibility contract as the gauge family: `role="img"`
with a title and description, text equivalents for series values and
thresholds, validated colours, and a reduced-motion variant. Hover tooltips are
optional and never required to read the chart.

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
