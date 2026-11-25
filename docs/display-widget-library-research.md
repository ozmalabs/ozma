# Display widget and theme library research

**Date:** 2026-11-25
**Status:** Research and recommendation. Not legal advice.
**Execution plan:** [`display-platform-execution-plan.md`](display-platform-execution-plan.md).

## Constraints

Ozma is `AGPL-3.0-only WITH OzmaPluginException`. Vendored assets must be
compatible with that licence and with commercial distribution. The browser
display adapter is deliberately dependency-free and offline-capable, and the
controller/native renderer must work without a Node build step.

Additional filters:

- MIT, BSD, ISC, Apache-2.0 are acceptable.
- LGPL is acceptable only with dynamic linking and a licence review.
- GPL-2.0-only, CC BY-NC, “personal use only”, and unspecified licences are not
  acceptable for vendored assets.
- Themes and widget packs are untrusted input. Do not execute third-party
  JavaScript in the controller or browser origin.
- AIDA64 itself and AIDA64 sensor-panel skins are proprietary; support importing
  a user's files, but do not bundle or redistribute them.

## Recommended stack

| Layer | Recommendation | Why |
|---|---|---|
| Browser widget runtime | Existing `web/display` vanilla renderer | No build step, no framework, already speaks the Display protocol |
| Time-series charts | **uPlot** (MIT, ~50 KB min, zero dependencies) | Smallest fast Canvas chart; ideal for sparklines and sensor history |
| Gauges/arcs/meters | Built-in SVG/CSS widgets | No dependency; matches the current `gauge`/`bar`/`vu_meter` schema |
| Dashboard layout editor | **gridstack.js** (MIT) if drag/drop is added | Small, framework-free, pointer/touch, responsive |
| Optional web components | **Lit** (BSD-3) only inside sandboxed widget packs | Home Assistant's card model; avoid making it core |
| Rust server-side PNG | **plotters** (MIT) | Existing controller renderer already produces PNGs server-side |
| Rust preview/editor | **egui** (MIT/Apache-2.0) or **iced** (MIT) | Native theme/widget editor without a web runtime |
| Embedded TFT | **LVGL** (MIT) | 30+ widgets, themes, 100 KB-class flash, touch/keypad |
| Embedded mono OLED/e-ink | **u8g2** (BSD-2) or **embedded-graphics** (MIT/Apache-2.0) | Small, mature, no OS requirement |
| Icons | **Lucide** (ISC), **Tabler** (MIT), **Material Symbols** (Apache-2.0) | Permissive, broad coverage, SVG |
| Fonts | **Inter**, **IBM Plex**, **JetBrains Mono** (SIL OFL 1.1) | Permissive, readable at panel sizes |

Do **not** make React, Vue, Tailwind, Grafana, or Home Assistant frontend a
runtime dependency of the node or controller. They are useful as design
references, not as embedded packages.

## Web libraries

| Library | Licence | Size/notes | Use |
|---|---|---|---|
| uPlot | MIT | ~50 KB min, zero deps, Canvas 2D time-series | Vendor for history/sparkline widgets |
| Chart.js | MIT | ~25 KB tree-shaken / 254 KB full | Optional; heavier than uPlot for sensor history |
| Apache ECharts | Apache-2.0 | ~100 KB+ | Too heavy for the default adapter; acceptable in a full dashboard build |
| D3 | ISC | Modular, large surface | Use only for bespoke visualisations, not core widgets |
| Lit | BSD-3 | Small web-component base | Optional sandboxed widget-pack runtime |
| gridstack.js | MIT | Drag/drop/responsive grid, no framework | Dashboard layout editor |
| Gridster / Muuri | MIT | Grid/masonry layout | Alternatives to gridstack |
| Gauge.js / Canvas Gauges / JustGage | MIT | Small gauge libraries | Reference implementations; built-in SVG gauges are preferred |
| Peity / sparkline | MIT | Tiny inline sparklines | Fallback if uPlot is not vendored |
| Frappe Charts | MIT | ~15 KB gzip, SVG | Lightweight static charts |
| Chartist | MIT | ~10 KB gzip, SVG | Simple static charts |

### Recommendation

Keep the core adapter dependency-free. Vendor **uPlot** as the only chart
runtime, behind a feature flag or a separate `widgets/` asset. Build gauges,
bars, status tiles, and meters with SVG/CSS rather than adding a gauge library.
If a visual layout editor is added, use **gridstack.js** and keep the saved
format as Ozma JSON, not gridstack-specific state.

## Native and embedded libraries

| Library | Licence | Use |
|---|---|---|
| LVGL | MIT | TFT/OLED/e-paper MCU and Linux panels; widget library, themes, animations, charts |
| u8g2 | BSD-2 | Monochrome OLED/LCD; character and simple graphics |
| embedded-graphics | MIT/Apache-2.0 | Rust `no_std` drawing primitives |
| egui | MIT/Apache-2.0 | Native preview/editor; immediate mode, easy embedding |
| iced | MIT | Native Rust app if an retained-mode UI is preferred |
| plotters | MIT | Server-side PNG charts and previews |
| ratatui | MIT | TUI diagnostics/status screens |
| Qt | LGPL-3.0/GPL/commercial | Only with dynamic linking and legal review; not recommended for static vendor |
| GTK | LGPL-2.1 | Dynamic linking acceptable; heavier than needed |
| Flutter | BSD-3 | Possible full client; large runtime, not needed for panels |
| Avalonia | MIT | .NET native UI alternative; not aligned with current Rust stack |
| Tauri | MIT/Apache-2.0 | Wrapper if a desktop shell is needed |
| Dioxus | MIT/Apache-2.0 | Rust web/native UI alternative; evaluate before adopting |

### Recommendation

Use **LVGL** for dedicated MCU/TFT panels and **plotters** for controller-side
PNG rendering. Use **egui** only for a local theme/widget editor. Do not add Qt
or a browser engine to the node.

## AIDA64 / SensorPanel ecosystem

| Source | Licence | Can we vendor? | Notes |
|---|---|---|---|
| AIDA64 application | Proprietary | No | EULA forbids copying, modification, or redistribution |
| AIDA64 official SensorPanel designs | Proprietary/free download | No | Downloadable does not mean redistributable |
| AIDA64 forum community panels | Per-author, frequently no explicit licence | No | Forum rules forbid copying/re-uploading others' work |
| Paid SensorPanel skins (BHSY etc.) | Commercial | No | Per-seat/redistribution restrictions |
| `oae/sensorpanel` | MIT | Yes, with notice | Go tool; native JSON themes and React themes; useful schema/device-profile reference |
| `codeengie/sensor-panel` | MIT | Yes, with notice | Rainmeter/AIDA64 sensor-panel skins; verify bundled third-party images/fonts |
| Rainmeter skins | Per-skin; often CC BY-NC-SA | Usually no for commercial | Avoid NC; require explicit per-skin permission |
| `awesome-dashboards` listings | Metadata only | N/A | Discovery source, not a licence grant |

**Safe approach:** support importing AIDA64 `.rslcd`/`.sensorpanel` files and
SensorPanel-style JSON as data, but never bundle the files or their images.
Store the imported theme under the user's own profile and retain a provenance
field. Do not copy AIDA64 or Rainmeter assets into an Ozma theme pack.

## Theme and widget sources

| Source | Licence | Use |
|---|---|---|
| MagicMirror² core | MIT | Module architecture, layout/position concepts, default module ideas |
| MagicMirror community modules | Usually MIT, per-module | Adapt concepts; verify each module's licence before copying code |
| Home Assistant frontend | Apache-2.0 | Card/dashboard architecture reference; not a public component library |
| Home Assistant custom cards | Often MIT/Apache-2.0, per-card | Adapt permissively licensed cards; avoid `ha-*` internals |
| Grafana core | AGPL-3.0-only | Do not vendor the app; use only as a UX reference |
| Grafana bundled plugins | Mixed; check `LICENSING.md` | Do not assume Apache-2.0 |
| Grafana community dashboards | Per-dashboard | Do not bulk-import without licence review |
| `oae/sensorpanel` native themes | MIT | Schema inspiration and theme pack structure |
| Rainmeter skins | Per-skin, often CC BY-NC-SA | Reference only unless the licence is permissive |
| Stream Deck icon packs | Per-pack | Verify commercial redistribution rights |
| WLED effects | EUPL-1.2 | Do not vendor without compatibility review |
| OpenRGB effects | GPL-2.0 | Avoid for vendored assets |

## Icon and font sources

| Asset | Licence | Notes |
|---|---|---|
| Lucide | ISC | Clean line icons, SVG, easy to vendor |
| Tabler Icons | MIT | Large outline set |
| Material Symbols | Apache-2.0 | Broad coverage; variable font available |
| Bootstrap Icons | MIT | Simple UI/status icons |
| Phosphor Icons | MIT | Multiple weights |
| Font Awesome Free | Icons CC BY 4.0, fonts SIL OFL 1.1, code MIT | Attribution required for icons |
| Inter / IBM Plex / JetBrains Mono / Rajdhani / Orbitron | SIL OFL 1.1 | Vendor the OFL text and attribution |

## Existing repository and shared-database work to reuse

Two existing pieces should become one asset distribution model.

### Local widget-pack manager

`controller/screen_widgets.py` already has:

- `WidgetPack` / `WidgetPackManager`
- `controller/widget_packs/{pack_id}/manifest.json`
- local `options.json` persistence
- `install_pack(manifest, files)` and `remove_pack`
- an `ozma_connect_url` field in the shipped `retro-gauges` manifest
- `docs/display-overlays.md` pack format with `permissions`
- `controller/sensor_descriptions.py`, which describes description packs as
  shareable through Connect “just like widget packs”
- `controller/plugin_api.py`, which already models plugin capabilities

It is local-only today. It has no index, version history, content hashes,
signature verification, licence/provenance fields, sync, or update flow.

### Shared device database precedent

`docs/routing/device-database.md` already defines the model to follow:

- a public, version-controlled, community-contributed catalog;
- a canonical database on Connect;
- hierarchical distribution: full database, category packs, per-entry download;
- `DatabaseSyncPolicy` (`full`, `detected`, `manual`), local cache, offline use;
- custom/private entries, public submission, and local override by `id`;
- provenance and confidence per entry and per field;
- matching and auto-detection;
- a DB API (`search`, `match`, `entry`, `categories`, `sync`, `stats`,
  `submit`);
- a contribution/review workflow.

The widget/theme repository should be a sibling of that system, not a second
incompatible one.

## Integrated asset repository design

One canonical repository covers widgets, themes, description packs, and
user-supplied AIDA64/SensorPanel imports:

```text
ozma-asset-repository/
  index.json                     # signed, versioned, searchable metadata
  packs/
    org.ozma.widgets.core-dark/
      manifest.json
      LICENSE
      NOTICE
      checksums.json
      preview.png
      assets/...
  categories/
    gauges.json
    audio.json
    noc.json
    aida64-imports.json
```

### Pack entry schema

Extend the existing manifest rather than inventing a separate one:

```yaml
AssetPackEntry:
  id: string                     # org.ozma.widgets.core-dark
  type: string                   # widget_pack | theme_pack | description_pack | import_pack
  name: string
  version: string                # semver
  schema: string                 # org.ozma.asset-pack/1
  author: string
  description: string
  license: string                # SPDX expression
  source_url: string?
  tags: string[]
  capabilities: string[]         # rendered_widget, data_provider, native_render, ...
  permissions: string[]          # internet, ha_read, mesh_read, camera_read
  min_ozma_version: string
  min_display_schema: string
  assets: AssetFile[]
  preview: string?
  provenance: Provenance
  confidence: string             # exact | approximate | community | template
  vibe: VibeProfile?             # design-language intent
  shape: ShapeSignature?         # geometry/shape signature
  coherence: CoherenceReport?    # pack-level design coherence
  signature: string              # detached signature over manifest+checksums
  published_at: string
  yanked: bool

AssetFile:
  path: string
  sha256: string
  size_bytes: uint
  mime_type: string
  executable: bool               # false for data-only themes
  sandbox: string?               # iframe | wasm | none

Provenance:
  type: string                   # original | imported | adapted | community
  source: string?
  author: string?
  method: string?
  notes: string?
```

`executable: false` and `sandbox: none` are required for data-only themes.
Executable widget packs must run in a sandboxed iframe or compiled WASM; they
must never execute in the controller/browser origin.

### Distribution and sync

Mirror the device-database policy:

```yaml
AssetSyncPolicy:
  mode: string                   # full | installed | manual | offline
  sync_interval_hours: uint      # default 24
  cache_size_mb: uint?
  include_previews: bool         # default true
  include_external_imports: bool # never upload; download only if user opted in
  verify_signatures: bool        # required when Connect signing is enabled
```

Controller behaviour:

- `full`: cache the index and all permitted packs.
- `installed`: cache only installed/subscribed packs and their dependencies.
- `manual`: fetch only on explicit install.
- `offline`: never contact Connect; serve from the local cache.
- Local cache remains `controller/widget_packs/` for compatibility.
- Store `index.json`, pack metadata, checksums, and provenance beside the
  existing `options.json`.
- A failed/expired signature fails closed; a yanked version is never installed
  automatically.
- AIDA64/SensorPanel imports are private by default and never uploaded unless
  the user explicitly submits a redistributable entry with a compatible licence.

### Shared database and API

On Connect, store searchable metadata in the shared database and pack payloads
in object storage. Minimum tables/models:

```text
asset_pack            id, type, name, description, license, source_url,
                      confidence, published_at, yanked
asset_version         pack_id, version, manifest_json, checksums_json,
                      signature, min_ozma_version, min_display_schema
asset_file            pack_id, version, path, sha256, size_bytes, mime_type,
                      executable, sandbox
asset_tag             pack_id, tag
asset_provenance      pack_id, type, source, author, method, notes
asset_report          pack_id, version, reason, reporter, created_at
```

Expose a controller/Connect API that mirrors the device DB shape:

```text
GET  /api/v1/asset-db/search?q=gauge&type=widget_pack
GET  /api/v1/asset-db/entry/{id}
GET  /api/v1/asset-db/versions/{id}
GET  /api/v1/asset-db/categories
POST /api/v1/asset-db/install/{id}?version=...
POST /api/v1/asset-db/sync
POST /api/v1/asset-db/submit/{id}
GET  /api/v1/asset-db/stats
```

Local controller equivalents should use the same paths under
`/api/v1/widget-packs` or `/api/v1/asset-db`; the existing
`WidgetPackManager` becomes the local install/cache backend.

### Integration with the existing pack manager

1. Keep `WidgetPack`/`WidgetPackManager` for installed packs.
2. Add an index/sync layer above it; do not replace the on-disk layout.
3. Add `license`, `provenance`, `sha256`, `signature`, `capabilities`,
   `permissions`, and `min_display_schema` to the manifest parser.
4. Add `install_from_repository(pack_id, version)`:
   - resolve the index entry;
   - verify licence allowlist;
   - verify signature and checksums;
   - enforce size/path limits;
   - extract atomically under a temporary directory;
   - write the existing `manifest.json`/`options.json` shape.
5. Add `update`/`rollback` by version and keep the previous version cached.
7. Add a provenance/report UI so users can report broken or malicious packs.
8. Reuse the device-database confidence vocabulary and submission workflow.

### AIDA64/SensorPanel imports

Treat imported AIDA64 `.rslcd`/`.sensorpanel` files as **private import packs**:

- parse geometry, labels, units, colours, and image references;
- convert to the Ozma data-only theme schema;
- keep the original file under the user's private cache only;
- never include it in the public index;
- allow submission only when the user attests a redistributable licence and
  confirms they own the assets.

This preserves interoperability without redistributing AIDA64, Rainmeter, or
third-party skin assets.


## Widget translation and interchange

“Translate widgets between implementations” is mostly a **declarative model**
problem, not a code-generation problem. No single off-the-shelf library
preserves sensor-panel intent across AIDA64, browser, controller PNG, LVGL, and
native Rust. The useful libraries cover only slices.

### What exists

| Library / format | Licence | What it preserves | Limits |
|---|---|---|---|
| **Vega-Lite** | BSD-3 | Declarative charts: marks, encodings, scales, legends, thresholds, faceting | Charts only; no gauges/meters/cards |
| **Vega** / **vl-convert** | BSD-3 | Lower-level visualisation spec; server-side SVG/PNG in Rust | Charts only; larger spec |
| **Adaptive Cards** | MIT | Cards: text, images, facts, columns, actions, input elements | No gauges/charts; card semantics only |
| **JSON Forms** | MIT | JSON Schema + UI Schema forms and validators | Forms/settings, not dashboards |
| **Style Dictionary** | Apache-2.0 | Design tokens across CSS, native, iOS, Android | Theme values, not widgets |
| **W3C Design Tokens (DTCG)** | format | Portable colour, spacing, typography, elevation tokens | No layout or data bindings |
| **Iconify** | MIT code; per-set icon licences | Icon lookup across many icon sets | Must check each icon set's licence |
| **SVG** | W3C standard | Portable vector gauges, arcs, bars, icons | No data binding or interaction semantics |
| **Open UI** | community spec | Shared semantics for common controls | Not a renderer or translator |
| **LVGL** | MIT | Embedded widgets, charts, themes | Target implementation, not a translator |
| **Qt UI Tools / GtkBuilder / Blueprint** | LGPL / various | Tool-specific UI XML | Not cross-toolkit |
| **W3C Widgets / Apache Wookie / OpenSocial / UIML / UsiXML / MARIA / IFML** | mixed / obsolete | Historical widget/interaction models | Obsolete, retired, or research-only; do not adopt |
| **Compose Multiplatform / Flutter / Slint / Haxe** | various | Cross-platform app frameworks or code transpilers | Not widget-schema translators |
| **Penpot** | MPL-2.0 | Open design source and file format | Design tool, not a runtime translator |
| **Figma APIs** | proprietary | Design extraction | Not open and not runtime |

**Conclusion:** use libraries at the edges, but define the interchange model in
Ozma.

### Ozma Widget IR

### Lossless import invariant

Every source importer MUST be lossless. The typed IR fields are a semantic
projection for rendering; the importer also retains the complete source
document, unknown fields, assets, fonts, effects, animations, scripts, and
format-specific extensions. Nothing is discarded on the way in.

`WidgetIR`:

```yaml
WidgetIR:
  id: string
  kind: string                  # gauge | bar | number | text | sparkline | chart |
                                # status | icon | table | card | custom
  role: string?                 # cpu_temp | fan_rpm | memory | threshold | ...
  binding: DataBinding          # metric key, source, unit, scale, format
  domain: ValueDomain?          # min, max, nominal, thresholds, severity bands
  history: HistoryPolicy?       # window, sample rate, aggregation
  label: string?
  icon: IconRef?
  actions: ActionRef[]?
  accessibility: Accessibility?
  layout: LayoutHint?           # size, position, zone, responsive priority
  style: StyleRef?              # token references, not raw target CSS
  typography: TypographyRef?    # font intent + size/weight/tracking/leading
  fonts: FontAsset[]?           # original/embedded/device font assets
  vibe: VibeProfile?            # intended design language
  shape: ShapeSignature?        # target-independent geometry signature
  source: SourceDocument        # lossless source record
  extensions: map[string, any]? # format-specific typed extensions
  assets: AssetRef[]?           # images, fonts, scripts, previews, raw source
```

`SourceDocument` keeps the source format's complete payload:

```yaml
SourceDocument:
  format: string                # aida64_rslcd | aida64_sensorpanel | rainmeter_ini |
                                # home_assistant_yaml | magicmirror_js | grafana_json |
                                # adaptive_card | ozma_display_definition
  version: string?
  source_hash: string
  original: AssetRef?           # original bytes, private by default
  parsed: map[string, any]?     # full format-specific parse
  unknown: map[string, any]?    # every field the typed IR does not model
```

`AssetRef` preserves non-semantic content without interpreting it:

```yaml
AssetRef:
  id: string
  path: string
  mime_type: string
  sha256: string
  size_bytes: uint
  kind: string                  # original | image | font | script | theme | preview | other
  executable: bool              # true for scripts/code
  sandbox: string?              # iframe | wasm | none
```

Executable scripts are preserved as opaque, hashed, sandboxed assets. They are
never executed by the controller or browser origin.

`DataBinding` should reuse the display protocol's field/definition identity,
so the same widget can be bound to `@active.TCPU`, an MQTT topic, or a Home
Assistant entity.

### Adapters

Imports are lossless. Exports are where loss is allowed, and every export MUST
produce a `TranslationReport`.

| Direction | Fidelity | Notes |
|---|---|---|
| AIDA64 `.rslcd` / `.sensorpanel` → IR | **lossless** | Parsed geometry into typed IR; raw file, images, fonts, animations, custom effects, unknown fields retained |
| Rainmeter `.ini` → IR | **lossless** | Parsed measures/meters plus raw INI, assets, scripts, and unknown keys retained |
| Home Assistant card YAML → IR | **lossless** | Typed card projection plus raw YAML, custom-card config, and assets retained |
| MagicMirror module config → IR | **lossless** | Known module semantics plus raw config, module JS, CSS, and assets retained |
| Grafana panel JSON → IR | **lossless** | Typed panel/query projection plus raw JSON and unknown fields retained |
| Adaptive Card JSON ↔ IR | **lossless** | Full card document and unknown fields retained |
| Ozma DisplayDefinition/ScreenLayout ↔ IR | **lossless** | Canonical target |
| IR → browser (SVG/CSS/uPlot) | export-dependent | Full fidelity for built-in types; report unsupported effects |
| IR → controller PNG (plotters/Vega-Lite) | export-dependent | Static previews/history charts; report unsupported interaction |
| IR → LVGL | export-dependent | Gauges/bars/labels/icons/charts; report simplified effects |
| IR → AIDA64 HTML/SSE | lossy | Numeric/text only; report dropped layout, images, effects |
| IR → Adaptive Card | export-dependent | Card subset; report dropped gauge/chart semantics |

The importers may choose not to *execute* unsupported scripts or custom code,
but they must still preserve it as an opaque asset. "Not executed" is not
"dropped".

### Fidelity model

Two reports, not one:

```yaml
SourceCaptureReport:
  source_format: string
  source_version: string?
  source_hash: string
  captured: string[]            # represented in typed IR
  opaque: string[]              # retained but not interpreted
  assets: string[]
  lossless: true                # MUST be true for import

TranslationReport:
  direction: export
  source_format: string
  target_format: string
  translator: string
  translator_version: string
  fidelity: exact | semantic | approximate | visual_only
  converted: string[]
  approximated: string[]
  dropped: string[]
  warnings: string[]
```

Export confidence:

| Confidence | Meaning |
|---|---|
| `exact` | Target represents the same semantic widget and properties |
| `semantic` | Same meaning and data binding; target-specific styling differs |
| `approximate` | Meaning preserved but some properties are inferred, simplified, or dropped |
| `visual_only` | Geometry/text preserved; behaviour or data binding lost |

Round-tripping source → IR → the same source format should be lossless when the
original/parsed data is retained. IR → a different target is allowed to lose
information, but the report must say exactly what was lost.

### Translation rules

- **Imports are lossless.** Keep unknown fields, assets, fonts, effects,
  animations, scripts, raw source, and format-specific extensions.
- **Exports are the only loss boundary.** Every export adapter emits
  `TranslationReport`.
- Do not prune data because a target cannot use it. The target adapter decides
  what to ignore and records it.
- Preserve semantic kind, binding, unit, domain, thresholds, label, icon,
  accessibility intent, and layout constraints.
- Preserve history policy where the target supports it; otherwise collapse to a
  current value and record the loss in the export report.
- Preserve theme intent as design tokens; do not copy target-specific CSS or
  proprietary assets.
- Keep the original imported file private; export only the converted IR and a
  provenance record.
- Round-trip tests on a golden corpus: source → IR → same source; and
  source → IR → target with a checked `TranslationReport`.
- AIDA64/Rainmeter skins with unknown licences remain private imports and are
  never published to the repository.

### Recommendation

1. Define lossless `WidgetIR`, `SourceDocument`, `SourceCaptureReport`, and
   export-only `TranslationReport` in the display schema.
2. Require every importer to retain raw source, unknown fields, and assets.
3. Use **Vega-Lite** as the chart/history IR and `vl-convert` for server-side
   SVG/PNG; use **Adaptive Cards** for card-subset import/export; use
   **JSON Forms** only for settings forms.
4. Use **DTCG tokens + Style Dictionary** for theme translation.
5. Write Ozma-specific importers for AIDA64/SensorPanel, Rainmeter, Home
   Assistant, MagicMirror, and Grafana; use a shared IR rather than N×M
   converters.
6. Render IR to browser SVG/uPlot, controller PNG, LVGL, AIDA64 HTML/SSE, and
   Adaptive Cards through small target adapters.
7. Store both `source_capture` (lossless) and export `translation_report`s with
   the pack entry so the UI can show what was preserved on import and what was
   lost on each export.

## Fonts and typography

Fonts are not just a family string. A widget’s appearance depends on metrics,
shape, coverage, hinting, which glyphs exist, and whether the font may legally
be embedded. Treat fonts as first-class assets and match them by **shape and
intent**, not by family name.

### Existing font systems to use

| System | Licence | Use |
|---|---|---|
| **fontconfig** | MIT-style | System font discovery, family/style matching, aliases, fallback |
| **FreeType** | FTL (or GPLv2) | Rasterisation, hinting, bitmap conversion |
| **HarfBuzz** | MIT | Shaping, script/language support, OpenType features |
| **Pango** | LGPL-2.1 | Text layout, bidi, fallback chains |
| **Skia** | BSD-3 | Font management and rendering (if a full renderer is used) |
| **fontdb** (Rust) | MIT | Local/system font database and family queries |
| **ttf-parser / skrifa / read-fonts** (Rust) | MIT/Apache-2.0 | Parse names, metrics, OS/2, PANOSE, axes, glyph coverage |
| **rustybuzz** (Rust) | MIT | HarfBuzz-compatible shaping |
| **fontdue / ab_glyph / swash** (Rust) | MIT/Apache-2.0 | Rasterise and measure glyphs |
| **cosmic-text / font-kit** (Rust) | MIT/Apache-2.0 | Higher-level layout and system font access |
| **CSS Font Loading API / FontFaceSet** | web standard | Browser font loading, fallback, OpenType features |
| **OpenType.js / fontkit / harfbuzzjs** | MIT | Browser/Node font inspection, shaping, subsetting |
| **fonttools / pyftsubset / woff2** | MIT | Subsetting, WOFF2 conversion, table inspection |
| **lv_font_conv / LVGL converter** | MIT | Generate LVGL C fonts with ranges, bpp, compression |
| **u8g2 / bdfconv** | BSD-2 | Monochrome OLED/LCD bitmap fonts |
| **Adafruit GFX fontconvert** | BSD | Small TFT bitmap fonts |
| **W3C DTCG + Style Dictionary** | format/Apache-2.0 | Typography tokens (family, size, weight, leading, tracking) |

### Font intent model

`FontIntent` carries what the source meant, independent of the original file:

```yaml
FontIntent:
  role: string                  # body | label | numeric | display | caption |
                                # icon | brand | code
  classification: string        # sans | serif | mono | display | script | icon
  traits:
    weight: uint?               # 100..900
    width: string?              # condensed | normal | expanded
    slant: string?              # upright | italic | oblique
    x_height: float?
    cap_height: float?
    contrast: string?           # low | medium | high
    roundness: string?          # geometric | humanist | grotesque
    aperture: string?           # open | closed
  scripts: string[]             # latin, latin-ext, cyrillic, greek, cjk, arabic, ...
  features: string[]            # tabular_nums, slashed_zero, ligatures, oldstyle
  target: TargetFontProfile?
  fallbacks: FontRef[]

TypographyRef:
  intent: string                # FontIntent id
  size_px: float?
  size_pt: float?
  line_height: float?
  tracking: float?
  weight: uint?
  features: string[]
```

`FontAsset` preserves the actual font and its provenance:

```yaml
FontAsset:
  id: string
  family: string
  style: string
  version: string?
  format: string                # ttf | otf | woff2 | bitmap | lvgl_c | bdf | pcf
  sha256: string
  source: string                # embedded | device | system | web | generated
  license: string
  embedding_allowed: bool
  subset: bool
  units_per_em: uint?
  ascent: int?
  descent: int?
  panose: string?
  os2_family_class: uint?
  variable_axes: map[string, float]?
  glyph_coverage: string[]?     # Unicode ranges or explicit codepoints
  features: string[]?
  private: bool                 # never publish without permission
```

### Device font profiles

Custom fonts are often device-specific. Every target declares constraints:

```yaml
TargetFontProfile:
  target: string                # browser | controller_png | lvgl | u8g2 | streamdeck |
                                # corsair_lcd | eink | aida64_html | adaptive_card
  formats: string[]             # ttf, otf, woff2, lvgl_c, bdf, pcf, bitmap
  dpi: uint?
  color_depth: uint?
  bpp: uint?                    # 1/2/4/8 for embedded fonts
  cell_width: uint?
  cell_height: uint?
  antialias: bool?
  hinting: bool?
  max_bytes: uint?
  scripts: string[]
  fallback_family: string?
```

Examples:

- **Stream Deck / Corsair LCD**: render text into key images; the device font is
  not exposed. Embed a permissively licensed TTF and rasterise at the exact key
  size.
- **LVGL**: convert to C with a bounded glyph range, bpp, optional compression;
  variable fonts are flattened/instanced first.
- **u8g2 / e-ink**: BDF/PCF or 1-bpp bitmap; high contrast, no subpixel
  antialiasing, smaller glyph sets.
- **AIDA64 HTML/SSE**: use CSS `@font-face` only when embedding is licensed;
  otherwise declare fallbacks and send text only.
- **Browser/controller PNG**: TTF/OTF/WOFF2, HarfBuzz shaping, fontconfig/fontdb
  fallback, CSS/typography tokens.

### Translation pipeline

1. **Capture losslessly.** Import the original font file, name table, OS/2,
   PANOSE, metrics, axes, coverage, and licence into the IR. Retain the file as
   a private `FontAsset`; no data is discarded on import.
2. **Resolve intent.** Derive role, classification, traits, scripts, features,
   size/weight/tracking/leading from the source usage and metadata.
3. **Check target capability.** If the original font is embeddable and the
   target supports its format, use it exactly.
4. **Match by shape.** If not, score candidate fonts:
   - role and classification;
   - PANOSE / `sFamilyClass` / `post` italic angle;
   - metric compatibility (units/em, x-height, cap-height, ascent/descent,
     average advance);
   - glyph/script coverage and OpenType features;
   - optional rendered-bitmap similarity at the target size.
5. **Fit metrics.** Adjust size, tracking, and leading so text occupies the same
   layout box; preserve hierarchy and alignment.
6. **Convert for the target.** WOFF2 for web, TTF/OTF for native, LVGL C,
   BDF/PCF, or rasterised glyph atlas for bitmap devices.
7. **Report.** Emit `FontTranslationReport` with original, replacement,
   matching score, dropped glyphs/features, and licence decision.

```yaml
FontTranslationReport:
  source_font: string
  target_font: string
  action: exact | subset | substituted | rasterized | unavailable
  shape_score: float?
  metrics: map[string, any]?
  dropped_glyphs: string[]
  dropped_features: string[]
  license_ok: bool
  warnings: string[]
```

Substitution is an export decision. The IR always retains the original font and
metadata, even when the target cannot use it.

### Font licensing

- Check the font’s `OS/2 fsType` embedding bits, OFL Reserved Font Name, and
  licence terms before embedding.
- Do not bundle OS/vendor fonts (Segoe UI, SF Pro, Helvetica, etc.) unless the
  licence explicitly permits redistribution.
- System fonts should be referenced as fallbacks, not copied into packs.
- Preserve copyright/licence/name tables when subsetting where the licence
  requires it.
- Store original imported fonts privately; publish only fonts with a clear
  redistributable licence and a `NOTICE`/licence file.
- Prefer OFL/Apache-2.0/CC0/BSD fonts for shipped themes.

### Recommendation

1. Treat fonts as assets, not just CSS strings.
2. Add `FontAsset`, `FontIntent`, `TypographyRef`, `TargetFontProfile`, and
   `FontTranslationReport` to the display schema.
3. Use fontconfig/fontdb for discovery, HarfBuzz/rustybuzz for shaping,
   FreeType/fontdue/ab_glyph for rasterisation, and lv_font_conv/u8g2 for
   embedded targets.
4. Match by shape and intent score, not family name; preserve metrics and
   report substitutions.
5. Keep imported fonts lossless and private; export only when licensing allows.
6. Add a font corpus to the golden tests: Latin, CJK, Arabic/Indic shaping,
   tabular numbers, icons, variable fonts, and 1-bpp/4-bpp embedded targets.

## Vibe, shape, and design coherence

Keywords alone are useful, but “vibe” needs a small, versioned taxonomy plus a
geometry signature so it can drive search, matching, coherence checks, and
translation without pretending to be objective.

### VibeProfile

```yaml
VibeProfile:
  taxonomy: string              # org.ozma.vibe/1
  keywords: string[]            # freeform, preserved exactly
  mood: string[]                # calm, alert, urgent, cozy, playful, serious, ...
  era: string?                  # aviation_1950s, hi_fi_1970s, synth_1980s, y2k,
                                # flat_2010s, glass_2020s, ...
  form: string[]                # circular, rounded, square, chamfered, hexagonal,
                                # organic, angular, segmented, dot_matrix,
                                # seven_segment, needle, wireframe, ...
  material: string[]            # brushed_metal, carbon_fiber, glass, neon, paper,
                                # eink, crt, vfd, plastic, wood, ...
  density: string?              # sparse, balanced, dense, dashboard
  color: string[]               # monochrome, phosphor_green, amber, neon, pastel,
                                # high_contrast, warm, cool, ...
  typography: string[]          # geometric_sans, humanist_sans, monospace,
                                # condensed, dot_matrix, seven_segment, display, ...
  motion: string[]              # static, subtle, animated, mechanical, sweep,
                                # pulse, glow, ...
  references: string[]          # aviation, aerospace, noc, industrial_control,
                                # racing, gaming, sci_fi, cyberpunk, steampunk,
                                # bauhaus, scandinavian, military, ...
  ornament: string[]            # minimal, skeuomorphic, flat, glassmorphism,
                                # neumorphism, brutalist, decorative, ...
  confidence: float             # 0..1
  provenance:
    source_keywords: string[]   # exactly as supplied by the source
    derived_keywords: string[]  # from geometry/palette/fonts/vision
    user_keywords: string[]     # human override, highest priority
    methods: string[]           # manifest, geometry, palette, font, vision, human
```

Rules:

- Source keywords and user keywords are preserved verbatim.
- Derived keywords carry provenance and confidence; they never overwrite user
  tags.
- The taxonomy is versioned. Unknown values are retained as freeform keywords,
  not dropped.
- Keywords describe intended design language, not a quality judgement.

### ShapeSignature

Capture the geometry that carries the look:

```yaml
ShapeSignature:
  widget_kind: string
  aspect_ratio: float?
  corner_radius_ratio: float?
  stroke_width_ratio: float?
  fill_ratio: float?
  symmetry: string?             # none | vertical | horizontal | radial
  arc_start_deg: float?
  arc_end_deg: float?
  tick_count: uint?
  segment_count: uint?
  needle_style: string?         # classic | modern | minimal | sword
  chart_mark: string?           # line | area | bar | dot | step
  texture: string?              # flat | noise | scanline | grid | glossy | matte
  elevation: string?            # flat | inset | raised | floating
  glow: bool?
  animation: string?            # none | needle | pulse | sweep | shimmer
```

This is derived from widget properties, SVG/CSS geometry, preview images, or
explicit source metadata. It is target-independent.

### Coherence scoring

```yaml
CoherenceReport:
  score: float                  # 0..1
  dimensions:
    mood: float?
    era: float?
    form: float?
    material: float?
    color: float?
    typography: float?
    density: float?
    motion: float?
  outliers: string[]            # widget/theme ids that diverge most
  conflicts: string[]           # e.g. skeuomorphic_needle + flat_glass
  suggestions: string[]
  confidence: float
```

Use coherence for:

- theme/pack search and “related packs”;
- mixing widgets in one scene;
- warnings when a scene unintentionally combines incompatible design
  languages;
- choosing a target style during translation;
- highlighting import drift after export.

It is advisory, never blocking. A user can intentionally mix styles.

### Extraction order

1. Explicit source metadata/manifest keywords.
2. User overrides.
3. Geometry/SVG/CSS shape descriptors.
4. Palette and font classification.
5. Optional local vision tagging of preview images.
6. Human review for published packs.

No cloud model is required for the first version. A controlled keyword
vocabulary plus geometry covers most widgets; vision is an optional enrichment.

## Proposed Ozma theme format

Use one data-only format for browser, native, and embedded renderers:

```json
{
  "schema": "org.ozma.theme/1",
  "id": "org.ozma.theme.core-dark",
  "name": "Core Dark",
  "author": "Ozma Labs",
  "license": "AGPL-3.0-only WITH OzmaPluginException",
  "provenance": {
    "source": "original",
    "inspired_by": ["aida64-sensorpanel", "home-assistant"]
  },
  "vibe": {
    "taxonomy": "org.ozma.vibe/1",
    "keywords": ["minimal", "dark", "technical", "calm"],
    "mood": ["calm", "technical"],
    "era": "flat_2010s",
    "form": ["rounded", "segmented"],
    "material": ["matte"],
    "density": "dashboard",
    "color": ["high_contrast", "cool"],
    "typography": ["geometric_sans", "monospace"],
    "ornament": ["minimal"]
  },
  "canvas": { "width": 680, "height": 480, "background": "#0a0a0f" },
  "tokens": {
    "color.bg": "#0a0a0f",
    "color.fg": "#e5e7eb",
    "color.accent": "#5b6fff",
    "color.warn": "#f0b94a",
    "color.crit": "#f26464"
  },
  "widgets": [
    {
      "type": "gauge",
      "x": 20, "y": 20, "w": 140, "h": 140,
      "metric": "@active.TCPU",
      "min": 0, "max": 100,
      "label": "CPU",
      "unit": "°C",
      "style": "arc"
    }
  ]
}
```

Rules:

- Themes are JSON, CSS variables, SVG, and images only.
- No executable JS in a theme. Widget packs that need code run in a sandboxed
  iframe with a strict CSP, or are compiled to WASM with a capability boundary.
- Every asset carries `license`, `source`, and attribution.
- The theme loader validates size, depth, asset paths, and MIME types.
- Importer output is normalised into this schema; the IR retains the raw source
  and unknown fields privately, while the shipped theme pack contains only
  converted data and cleared assets.
- Provide a **theme pack** container with a manifest, licence, preview image,
  and asset hashes.

## Licensing policy for vendoring

Create `third_party/` with:

```text
third_party/
  README.md
  LICENSES/
    MIT.txt
    BSD-3-Clause.txt
    Apache-2.0.txt
  uplot/
    LICENSE
    NOTICE
    uPlot.iife.min.js
```

Allowlist:

- MIT, BSD-2/3, ISC, Apache-2.0, SIL OFL 1.1, CC BY 4.0 (with attribution).
- LGPL only with dynamic linking and a recorded review.
- Reject GPL-2.0-only, AGPL for components we do not want to relicense, CC
  BY-NC, “personal use only”, and unspecified licences.

Automate with a dependency-licence check in CI and a generated
`THIRD_PARTY_NOTICES.md`.

## Recommended next steps

1. **Extend the existing widget-pack manager into the asset repository.** Add
   index/sync, version/rollback, hashes, signatures, licence/provenance, and
   category packs to `controller/screen_widgets.py`; do not replace its on-disk
   layout.
2. Add a shared `AssetSyncPolicy` and the `/api/v1/asset-db/*` endpoints,
   modelled on the device-database sync and API.
3. Define lossless `WidgetIR`, `SourceDocument`, `SourceCaptureReport`, and
   export-only `TranslationReport`, then add importers for AIDA64/SensorPanel,
   Rainmeter, Home Assistant, MagicMirror, and Grafana. Imports must retain raw
   source, unknown fields, and assets.
4. Add `FontAsset`, `FontIntent`, `TargetFontProfile`, and
   `FontTranslationReport`; integrate fontconfig/fontdb, HarfBuzz/rustybuzz,
   FreeType/fontdue/ab_glyph, and lv_font_conv/u8g2. Match fonts by shape and
   intent, not family name.
5. Add `VibeProfile`, `ShapeSignature`, and `CoherenceReport`; preserve source
   and user keywords, derive shape/geometry tags with confidence, and add
   repository search/related-pack/coherence checks.
6. Use Vega-Lite/`vl-convert` for chart IR, Adaptive Cards for card-subset
   interchange, JSON Forms for settings, and DTCG tokens + Style Dictionary for
   themes.
7. Vendor **uPlot** behind a `web/display/widgets/` asset and add a `sparkline`
   renderer that uses it when present.
8. Add a data-only **theme schema** and loader; ship 3–5 original Ozma themes
   (`core-dark`, `core-light`, `noc`, `audio`, `minimal`).
9. Add an **AIDA64/SensorPanel importer** for user-owned files. Parse geometry,
   colours, labels, units, and images into typed IR, and retain the raw file and
   assets as a lossless private import. Do not execute or redistribute assets;
   submission requires an attested compatible licence.
10. Add an **SVG gauge widget** family (arc, bar, dual-bar, threshold) to the
   existing schema so no gauge dependency is needed.
11. Add **LVGL** export for MCU panels and **plotters** export for controller
    PNG previews from the same theme document.
12. Add provenance/licence fields to widget packs and a CI licence gate.

## Sources

- uPlot — <https://github.com/leeoniya/uPlot>
- Chart.js — <https://www.chartjs.org/>
- Apache ECharts — <https://echarts.apache.org/>
- D3 — <https://d3js.org/>
- Lit — <https://lit.dev/>
- gridstack.js — <https://gridstackjs.com/>
- LVGL — <https://lvgl.io/> and <https://github.com/lvgl/lvgl>
- u8g2 — <https://github.com/olikraus/u8g2>
- embedded-graphics — <https://github.com/embedded-graphics/embedded-graphics>
- egui — <https://github.com/emilk/egui>
- iced — <https://github.com/iced-rs/iced>
- plotters — <https://github.com/plotters-rs/plotters>
- MagicMirror² — <https://github.com/MagicMirrorOrg/MagicMirror>
- Home Assistant frontend — <https://github.com/home-assistant/frontend>
- Grafana licensing — <https://grafana.com/licensing/>
- AIDA64 external applications — <https://www.aida64.com/user-manual/hardware-monitoring/external-applications>
- AIDA64 SensorPanel — <https://www.aida64.com/aida64-sensorpanel>
- `docs/routing/device-database.md` — canonical repository, Connect distribution,
  sync policy, provenance/confidence, and submission model
- `controller/screen_widgets.py` — existing `WidgetPackManager`
- `docs/display-overlays.md` — existing pack format and permission model
- `oae/sensorpanel` — <https://github.com/oae/sensorpanel>
- `codeengie/sensor-panel` — <https://github.com/codeengie/sensor-panel>
- Rainmeter skin distribution guidance — <https://docs.rainmeter.net/manual/distributing-skins/>
- Lucide — <https://lucide.dev/>
- Tabler Icons — <https://tabler.io/icons>
- Material Symbols — <https://fonts.google.com/icons>
- Vega-Lite — <https://vega.github.io/vega-lite/>
- vl-convert — <https://github.com/vega/vl-convert>
- Adaptive Cards — <https://adaptivecards.io/>
- JSON Forms — <https://jsonforms.io/>
- Style Dictionary — <https://styledictionary.com/>
- W3C Design Tokens format — <https://www.designtokens.org/tr/drafts/format/>
- Iconify — <https://iconify.design/>
- Open UI — <https://open-ui.org/>
- fontconfig — <https://fontconfig.org/>
- FreeType — <https://freetype.org/>
- HarfBuzz — <https://harfbuzz.github.io/>
- Pango — <https://pango.gnome.org/>
- fontdb — <https://github.com/RazrFalcon/fontdb>
- ttf-parser — <https://github.com/RazrFalcon/ttf-parser>
- rustybuzz — <https://github.com/harfbuzz/rustybuzz>
- fontdue — <https://github.com/mooman219/fontdue>
- ab_glyph — <https://github.com/alexheretic/ab-glyph>
- cosmic-text — <https://github.com/pop-os/cosmic-text>
- OpenType.js — <https://opentype.js.org/>
- fontkit — <https://github.com/foliojs/fontkit>
- fonttools — <https://fonttools.readthedocs.io/>
- lv_font_conv — <https://github.com/lvgl/lv_font_conv>
- u8g2 font tools — <https://github.com/olikraus/u8g2/wiki/u8g2fontformat>
