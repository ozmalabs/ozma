# Ozma layout editor

A dependency-free, offline editor for Ozma `ScreenLayout` documents. It has no
npm dependencies, no build step, no vendored layout library, and no network
access. Open [`index.html`](index.html) from a local static server:

```bash
cd web/display/editor
python3 -m http.server 8081
```

Then open <http://localhost:8081/>. Browsers block ES modules on `file://`, so
serve the directory rather than opening the HTML directly.

## What it does

- Loads, pastes, or imports a `ScreenLayout` or display-definition JSON
  document. A wrapped definition (`{ "layout": ... }`) and a
  `definition_json` payload are accepted; the editor saves only the
  `ScreenLayout`.
- Preserves unknown top-level fields, `extensions`, unknown widget fields, and
  unrelated JSON keys. When a loaded string needs no normalisation it
  round-trips byte-for-byte until the first edit.
- Draws the existing widget kinds (`gauge`, `bar`, `number`, `text`,
  `sparkline`, `vu_meter`, `status`, `icon`, `table`, `card`) as positioned
  rectangles with a small preview.
- Drag to move, drag any of eight handles to resize, snap to the grid, clamp to
  canvas bounds, raise/lower z-order, duplicate, and delete.
- Edit label, text, metric, unit, min/max, warn/crit thresholds, colour,
  decimals, geometry, sparkline history points/seconds/fill, and VU
  attack/release/falloff/peak fields.
- Undo and redo (bounded history), keyboard nudge, Delete, and Ctrl/Cmd+Z.
- Toolbar with add, grid size, zoom, import, export/download, and reset.
- Status and error regions announce changes and failures.

The editor never evaluates imported data. Widget labels, metrics, and text are
written with `textContent`; they are not parsed as HTML. Colours are restricted
to colour literals before they are assigned to CSS, so an imported
`url(...)`/`javascript:` value cannot trigger a load or execution.

## Model API

`editor.js` exports a pure model used by `node:test`:

| Export | Purpose |
|---|---|
| `createEditorState(input?, settings?)` | New empty editor, or parse an input document. Settings are `grid`, `zoom`, and `historyLimit`. |
| `parseLayout(input, settings?)` | Parse a JSON string/object/wrapped definition into state. |
| `serializeLayout(state)` | Return the `ScreenLayout` JSON string. Unedited source text is returned exactly. |
| `widgetDefaults(kind, overrides?)` | Fresh default widget for a supported kind. |
| `getWidgetId(widget)` | Opaque editor selection id for a widget (never serialised). |
| `snapRect(rect, grid, bounds?)` | Snap and clamp a rectangle. |
| `addWidget(state, kind, options?)` | Add and select a widget. |
| `removeWidget(state, id?)` | Remove a widget (defaults to the selection). |
| `moveWidget(state, id, x, y)` | Move with grid snapping and bounds clamping. Accepts `{ x, y }`. |
| `resizeWidget(state, id, rectOrWidth, height?)` | Resize with grid snapping, minimum size, and bounds clamping. |
| `selectWidget(state, id)` | Select a widget or clear with `null`. |
| `updateWidget(state, id, patch)` | Update allowlisted properties; unknown fields are untouched. |
| `setLayoutMeta(state, patch)` | Update id/name/size/background/refresh rate. |
| `setGrid(state, value)` / `setZoom(state, value)` | Clamp view settings; never serialised. |
| `undo(state)` / `redo(state)` | Restore snapshots within the history limit. |
| `duplicateWidget`, `raiseWidget`, `lowerWidget`, `resetLayout` | Editing helpers. |
| `sanitizeColor(value, fallback?)` | Accept safe colour literals only. |

Internal selection ids are non-enumerable, so they never appear in exported
JSON. Layout state and widget objects are cloned per edit; the exported helpers
do not mutate the previous state.

## Accessibility

- Widget boxes are focusable (`tabindex="0"`). Tab moves between them.
- Enter or Space selects; arrows nudge by one grid step (Shift+arrows by five);
  Delete/Backspace removes; Escape clears the selection.
- Visible `:focus-visible` outlines and labelled form controls.
- Status changes use `role="status"` plus `aria-live="polite"`; errors use
  `role="alert"`.
- `prefers-reduced-motion: reduce` disables all transitions and animations.

## Security

`index.html` sets this CSP:

```text
default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline';
img-src 'self' data:; object-src 'none'; base-uri 'none'; form-action 'none'
```

There are no remote scripts, fonts, styles, or images. Downloads are produced
from an in-memory `Blob`; imports are read from a pasted string or a user-chosen
local file. The source contains no `eval`, dynamic function construction, or
HTML-string injection.

## Tests

```bash
node --check web/display/editor/editor.js
node --test web/display/editor/editor.test.mjs
node --test web/display/*.test.mjs web/display/widgets/*.test.mjs
```

The editor tests cover defaults for all widget kinds, add/remove/select,
move/resize/snap/clamp, duplicate and z-order, bounded undo/redo with redo
invalidation, VU configuration, parse/serialise preservation, grid/zoom
clamping, and static CSP/security/accessibility/DOM-wiring checks. DOM drag
behaviour is a browser smoke surface rather than a Node unit test.

## Scope

This editor saves `ScreenLayout` only. It does not write a third-party grid
editor's internal state, a compact `meta.ozma_ir` projection, theme packs, or
executable widget code.
