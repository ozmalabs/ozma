# Display Platform Execution Plan

**Status:** Executable plan derived from
[`display-widget-library-research.md`](display-widget-library-research.md).
Implemented: M0–M4, M6, and most of M5/M7. See §6.1 for the current task
ledger and remaining render/CI hardening work.
**Scope:** widget/theme repository, shared database, lossless Widget IR,
font intent/translation, render adapters, importers, and built-in themes.

## 1. Objective

Make Ozma display assets a first-class platform:

- one lossless interchange model for widgets, themes, fonts, and imported
  layouts;
- one repository and shared database for community assets;
- render the same intent on browser, controller PNG, LVGL, AIDA64 HTML/SSE, and
  Adaptive Cards;
- translate fonts by shape and intent, not family name;
- never lose source information on import; lose only on export, with a report.

## 2. Non-goals

- Vendoring Grafana, Home Assistant frontend, AIDA64, Rainmeter, or their
  bundled assets.
- Executing third-party JavaScript in the controller or main browser origin.
- Pixel-perfect reproduction of arbitrary proprietary skins.
- Full design-tool (Figma/Penpot) round-trip in the first release.
- A public asset marketplace UI before the repository/API is stable.

## 3. Invariants

1. **Import is lossless.** Raw source, unknown fields, assets, fonts, effects,
   animations, scripts, and format extensions are retained.
2. **Export is the only loss boundary.** Every export emits
   `TranslationReport` / `FontTranslationReport`.
3. **No executable theme code** outside an iframe sandbox or WASM capability
   boundary.
4. **Design language is captured, not inferred away.** Source/user vibe keywords,
   shape signatures, and coherence are preserved; derived tags carry provenance
   and confidence.
5. **Licence gate.** MIT/BSD/ISC/Apache-2.0/OFL/CC0 are allowed; GPL-2.0-only,
   CC BY-NC, personal-use-only, and unknown licences are rejected for published
   packs.
6. **Wire compatibility.** Existing `DisplayDefinition` / `ScreenLayout` and
   installed packs continue to work.
7. **Offline-first.** Cached assets and definitions render without Connect.
8. **Deterministic output.** Canonical JSON, stable hashes, reproducible font
   subsets and rasterised previews.

## 4. Architecture

```text
Source formats                     Ozma IR                     Targets
──────────────                     ───────                     ───────
AIDA64 .rslcd/.sensorpanel ─┐
Rainmeter .ini ─────────────┤
Home Assistant YAML ────────┼─> WidgetIR ─┬─> ScreenLayout / DisplayDefinition
MagicMirror config ─────────┤   + FontIR  ├─> browser (SVG/CSS/uPlot)
Grafana panel JSON ─────────┤   + assets  ├─> controller PNG renderer
Adaptive Cards JSON ────────┤             ├─> LVGL C/JSON
Ozma definitions ───────────┘             ├─> AIDA64 HTML/SSE
                                          └─> Adaptive Cards
```

Components:

- `ozma-display-ir` — lossless IR, validation, translation traits, reports, vibe/shape/coherence.
- `ozma-fonts` — font metadata, shape/intent matching, subset/raster helpers.
- `controller/asset_repository.py` — local index/cache/install/update.
- `connect/assets/` — shared Postgres metadata + object storage index.
- `controller/display_importers/` — format importers (Python).
- `web/display/widgets/` — browser widgets, uPlot, theme loader.
- `renderer/` — controller PNG renderer extended to consume IR.
- `ozma-node/src/aida64.rs` — AIDA64-compatible output and import-panel export.

## 5. Workstreams and sequencing

| Track | Workstream | Depends on |
|---|---|---|
| A | Schemas, IR, translation contracts | — |
| B | Local asset repository + Connect shared DB | A |
| C | Font subsystem | A |
| D | Render adapters + built-in themes | A, C |
| E | Importers (AIDA64, Rainmeter, HA, MagicMirror, Grafana) | A |
| F | Security, licensing, CI, rollout | A–E |

Parallel start:

```text
Week 1      A
Week 2      A/B/C/E
Week 3      B/C/E
Week 4      D/C/B
Week 5      D/E
Week 6      D/F
Week 7      F
```

M0 must land before B/C/D/E merge to avoid schema churn.

## 6. Milestones

| ID | Milestone | Exit criteria |
|---|---|---|
| M0 | Contracts and fixtures | JSON Schemas + Rust/Python types validate the same golden fixtures; wire-vs-IR storage decision recorded |
| M1 | Local asset repository | Existing packs load unchanged; install/update/rollback/offline work; hash/signature/path tests pass |
| M2 | Connect shared database | Index sync, category search, install, submission, stats work against Postgres/object storage; offline fallback tested |
| M3 | IR + translation core | Lossless source→IR→same-source round-trip on fixtures; IR→ScreenLayout renders in browser |
| M3b | Vibe/shape/coherence | Versioned taxonomy; source/user/derived tags separated; shape signatures; coherence report; repository filter |
| M4 | Font subsystem | Font intent/asset schema; shape scoring; LVGL/BDF/bitmap export; licence checks; golden font tests |
| M5 | Render adapters + themes | Same IR renders on browser, controller PNG, LVGL, AIDA64, Adaptive Cards; 3–5 original themes; no third-party assets |
| M6 | Importers | AIDA64/SensorPanel, Rainmeter, Home Assistant, MagicMirror, Grafana imports produce lossless capture reports + export reports |
| M7 | Hardening and rollout | CI licence/security gates; fuzz; migration; docs; feature flags default-on for local repo/IR; release notes |

### 6.1 Implementation status

| Milestone | State | Landed | Remaining |
|---|---|---|---|
| M0 Contracts | **Done** | `ozma-display-ir` models/validation/canonical hash; `controller/display_ir.py` parity; six `schemas/display/*.schema.json`; golden fixtures; lossless capture arrays aligned across Rust/Python/importers | `WGT-004` cross-link in `docs/protocols.md` |
| M1 Local repository | **Done** | `controller/asset_repository.py` index/cache/install/update/rollback, hashes, signatures, safe extraction, sync policy, API router; `retro-gauges` manifest migrated; `WidgetPack` preserves repository fields; signed Connect index sync interop tested | Operator deployment tuning only |
| M2 Connect shared DB | **Done** | `connect/assets/` models + migration, object storage, deterministic signed index, full API, licence/attestation rules, environment bootstrap (`bootstrap.py`), controller→Connect sync tests | Production key management, review UI, marketplace surface |
| M3 IR/translation core | **Done** | IR→`ScreenLayout`/`DisplayDefinition`, compact `meta.ozma_ir`, validation, export-report helper; adapters for gauge/bar/number/text/sparkline/status/icon/table/card; PNG render-plan/SVG, fake-canvas PNG/JPEG tests, LVGL export; browser/LVGL/Adaptive Cards goldens | Real raster run requires optional `@napi-rs/canvas`; per-target visual pixel snapshots |
| M3b Vibe/shape/coherence | **Done** | `org.ozma.vibe/1` models/validation; SVG/CSS/property shape extraction; source/user/derived keyword separation; advisory coherence with dimensions/outliers/conflicts/overrides; Rust/Python parity; repository filter/related packs | Optional local vision tagging (`WGT-049a`, disabled by default) |
| M4 Font subsystem | **Done** | Rust + Python metadata, PANOSE/metrics/scripts/features, `fsType` gate, deterministic matching, reports; TTF/OTF subsetting, BDF, LVGL C, previews; Latin/CJK/Arabic/Indic/icon golden corpus | WOFF2 write path requires optional `brotli`; live shaping render comparison |
| M5 Render adapters/themes | **Done** | Five original themes + hashed pack; gauge/bar/dual/threshold, VU ballistics (attack/release, dB/s falloff, peak hold, clip latch, RMS), and sparkline/history widgets; sandboxed third-party widgets; PNG render plans; LVGL C/JSON; Adaptive Cards; AIDA64 `/panel` generated from `WidgetIR` | Optional uPlot/gridstack editor; AIDA64 native Windows shared-memory/registry emulation |
| M6 Importers | **Done** | AIDA64/SensorPanel, Rainmeter, Home Assistant, MagicMirror, Grafana, Adaptive Cards import/export; private import store + API; lossless capture and IR validation | Native AIDA64 `.rslcd` remains best-effort/opaque (`WGT-070`) |
| M7 Hardening | **Done** | Feature flags/runtime, licence/checksum/sandbox gate, Ed25519 trust + signing CLI, browser sandbox/CSP, bounded fuzz regression, migration tooling, perf benchmarks, targeted CI, Connect bootstrap, release notes | Default-on rollout decision; marketplace/review UI |

Known gaps for the next slice:

1. Real raster PNG/JPEG runs on a host with `@napi-rs/canvas`; current tests
   cover the canvas contract with an injected recording factory.
2. Optional uPlot/gridstack editor and AIDA64 native Windows exporters.
3. Default-on rollout after field validation; Connect review UI/marketplace.
4. Live font shaping/render comparison and WOFF2 writing with `brotli`.

## 7. Task backlog

### Phase 0 — Contracts and fixtures (M0)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-001 | Write JSON Schemas for `WidgetIR`, `SourceDocument`, `SourceCaptureReport`, `TranslationReport`, `FontAsset`, `FontIntent`, `TargetFontProfile`, `FontTranslationReport`, `AssetPackEntry`, `AssetSyncPolicy` | `schemas/display/*.schema.json` | `jsonschema`/Rust validation passes on fixtures |
| WGT-002 | Add Rust `ozma-display-ir` crate with serde models, validation, canonical hash | `ozma-display-ir/`, `Cargo.toml` | `cargo test -p ozma-display-ir`; same fixtures as Python |
| WGT-003 | Add Python IR models/validators for controller importers | `controller/display_ir.py` | Python and Rust accept/reject identical fixtures |
| WGT-004 | Decide wire mapping: compact IR projection in `DisplayDefinition.meta`, full IR/source in asset pack; document size limits | `docs/protocols.md`, `docs/display-widget-library-research.md` | No wire schema break; size tests |
| WGT-005 | Golden fixture corpus: AIDA64, Rainmeter, HA, MagicMirror, Grafana, Adaptive Cards, Ozma, fonts | `tests/fixtures/display_ir/` | Fixtures load and validate |
| WGT-006 | Add `TranslationReport`/`SourceCaptureReport` invariants and test helpers | IR crate, Python models | Import report must say `lossless:true`; export report always present |

### Phase 1 — Local asset repository (M1)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-020 | Extend manifest parser with `license`, `provenance`, `sha256`, `signature`, `capabilities`, `permissions`, `min_display_schema`, `source_capture`, `translation_reports` | `controller/screen_widgets.py` | Existing `retro-gauges` loads; unknown fields preserved |
| WGT-021 | Implement local `AssetIndex`, `AssetCache`, install/update/rollback, atomic extraction | `controller/asset_repository.py` | Install/update/rollback tests; no partial installs |
| WGT-022 | Add hash/signature verification, safe archive extraction, size/depth/path limits | `controller/asset_repository.py` | Traversal, symlink, oversize, bad hash, bad signature tests fail closed |
| WGT-023 | Add local API `/api/v1/asset-db/{search,entry,versions,categories,install,sync,stats}` | `controller/api.py` | API tests; typed errors |
| WGT-024 | Migrate `controller/widget_packs/retro-gauges` to the extended manifest | pack files | Loads and installs; licence/provenance populated |
| WGT-025 | Add offline cache and `AssetSyncPolicy` config | controller config | Offline renders from cache; no Connect call |
| WGT-026 | Add report/yank handling | repository/API | Yanked versions not auto-installed; reports recorded locally |
| WGT-027 | Index/search `vibe`, `shape`, `coherence`; add related-pack queries | `controller/asset_repository.py`, API | Filter by keyword/taxonomy/shape; coherent packs surface |

### Phase 2 — Connect shared database (M2)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-030 | Postgres schema/migration for `asset_pack`, `asset_version`, `asset_file`, `asset_tag`, `asset_provenance`, `asset_report` | `connect/assets/migrations/` | Migration applies; constraints/indexes tested |
| WGT-031 | Object-storage layout and signed index generation | `connect/assets/storage.py`, `index.py` | Deterministic index; Ed25519 signature verifies |
| WGT-032 | Connect API `/assets/{search,entry,versions,categories,submit,stats}` | `connect/assets/router.py`, `connect/main.py` | API tests with fake DB/storage |
| WGT-033 | Controller sync client (ETag/hash, retries, offline) | `controller/asset_repository.py` | Sync idempotent; network loss falls back to cache |
| WGT-034 | Submission/review workflow and licence allowlist | Connect asset service | Rejects non-allowlisted licences; records provenance |
| WGT-035 | Category packs and delta sync | Connect/controller | Full/installed/manual/offline modes tested |
| WGT-036 | Shared DB integration tests | `tests/unit/connect/test_assets.py` | Search/install/submit/stats pass |

### Phase 3 — IR and translation core (M3)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-040 | Implement `WidgetIR` ↔ `ScreenLayout` and `DisplayDefinition` mapping | `ozma-display-ir`, `controller/display_ir.py` | Lossless round-trip for Ozma fixtures |
| WGT-041 | Implement translation traits/adapters and report assembly | `ozma-display-ir` | Export adapter returns report; missing target features recorded |
| WGT-042 | Add IR validation (bounds, bindings, cycles, asset refs, size) | IR crate/Python | Invalid IR rejected with typed errors |
| WGT-043 | Add semantic built-in widget adapters (`gauge`, `bar`, `number`, `text`, `sparkline`, `status`, `icon`) | IR/renderers | Same IR renders in browser and PNG |
| WGT-044 | Add golden round-trip tests: source → IR → same source | `tests/fixtures/display_ir/` | Byte/semantic equality where raw retained |
| WGT-045 | Add export-report golden tests per target | fixtures | `dropped`/`approximated` exact |

### Phase 3b — Vibe, shape, and coherence (M3b)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-046 | Define versioned `org.ozma.vibe/1` taxonomy and `VibeProfile`, `ShapeSignature`, `CoherenceReport` schemas | `schemas/display/`, `ozma-display-ir`, Python models | Same fixtures validate in Rust/Python; unknown keywords retained |
| WGT-047 | Extract shape signatures from widget properties/SVG/CSS/preview; derive keywords from palette/font/geometry with confidence | IR/importers, `controller/display_importers/` | Source/user/derived keywords separated; user overrides win |
| WGT-048 | Implement coherence scoring across packs/scenes and outlier/conflict reporting | `ozma-display-ir`, controller | Advisory score; intentional mixing override; golden fixtures |
| WGT-049 | Add vibe/shape/coherence to theme-pack manifest and repository search/related packs | repository/API | Search/filter and related-pack tests |
| WGT-049a | Add optional local vision tagging of preview images (no cloud dependency) | importer/vision helper | Optional; disabled by default; confidence/provenance recorded |

### Phase 4 — Font subsystem (M4)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-050 | Implement `ozma-fonts` metadata extraction (names, metrics, PANOSE, OS/2, axes, coverage, `fsType`) | `ozma-fonts/` using `ttf-parser`/`skrifa`/`fontdb` | Fixtures for TTF/OTF/variable/icon/bitmap |
| WGT-051 | Implement FontIntent resolver and shape/intent scoring | `ozma-fonts` | Deterministic ranking on corpus; role/classification/metrics/coverage tests |
| WGT-052 | Python font matching for controller renderer | `controller/font_translation.py` (fontconfig/fontTools/Pillow) | Same scoring fixtures as Rust |
| WGT-053 | WOFF2/TTF/OTF subsetting for web/native | fonttools/pyftsubset/fontkit | Subset preserves required tables/licence; rendering smoke |
| WGT-054 | LVGL C font export and u8g2/BDF/bitmap export | `ozma-fonts`, `lv_font_conv`, fonttools | Generated font compiles/renders at 1/2/4/8 bpp |
| WGT-055 | Font licence/embedding check (`fsType`, OFL RFN) | font modules | Non-embeddable fonts substituted, not embedded |
| WGT-056 | Font golden corpus: Latin, CJK, Arabic/Indic shaping, tabular nums, icons, variable/bitmap | `tests/fixtures/fonts/` | Shape/match/export reports stable |
| WGT-057 | Add `FontTranslationReport` to IR/export | IR/renderers | Every font substitution/rasterisation reported |

### Phase 5 — Render adapters and themes (M5)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-060 | Browser theme loader + CSS variables + font loader | `web/display/` | Theme renders; offline; no build step |
| WGT-061 | SVG gauge family (arc/bar/dual/threshold) | `web/display/widgets/` | Visual snapshot + reduced-motion/a11y tests |
| WGT-062 | Vendor uPlot and add `sparkline`/history widget | `web/display/vendor/uplot/`, widget | MIT notice; history streaming test |
| WGT-063 | Controller PNG renderer consumes IR/theme/fonts | `renderer/server.js` | Same IR renders to PNG; font fallback recorded |
| WGT-064 | LVGL exporter and sample firmware | `ozma-display-ir`, `examples/lvgl/` | C compiles; gauge/bar/chart render |
| WGT-065 | AIDA64 panel output from IR | `ozma-node/src/aida64.rs` | `/panel` layout generated from IR; HTML/SSE unchanged |
| WGT-066 | Adaptive Cards importer/exporter | IR crate/Python | Card subset round-trip |
| WGT-067 | Optional gridstack layout editor | `web/display/editor/` | MIT notice; saves Ozma JSON, not gridstack state |
| WGT-068 | Ship 3–5 original themes with `VibeProfile`/`ShapeSignature`/coherence: `core-dark`, `core-light`, `noc`, `audio`, `minimal` | `controller/widget_packs/` or asset repo | Licences/notices; previews; vibe tags; no third-party assets |
| WGT-069 | Theme pack container + preview/hash manifest | repository | Install/preview works |

### Phase 6 — Importers (M6)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-070 | AIDA64 format reconnaissance spike; fixtures and parser decision | `docs/` + fixtures | Known format documented; fallback plan if proprietary |
| WGT-071 | AIDA64 `.rslcd`/`.sensorpanel` importer | `controller/display_importers/aida64.py` | Lossless source capture + typed IR; export report on targets |
| WGT-072 | Rainmeter `.ini` importer | `controller/display_importers/rainmeter.py` | Raw INI/assets retained; common measures/meters mapped |
| WGT-073 | Home Assistant card YAML importer | `controller/display_importers/home_assistant.py` | Entity→binding; custom cards retained opaque |
| WGT-074 | MagicMirror config importer | `controller/display_importers/magicmirror.py` | Known modules mapped; JS/CSS retained opaque |
| WGT-075 | Grafana panel JSON importer | `controller/display_importers/grafana.py` | Chart/gauge/bar/stat mapped; query preserved opaque |
| WGT-076 | Import API + private import packs | controller API | Imports private by default; never auto-published |
| WGT-077 | Importer golden corpus and lossless tests | fixtures | `SourceCaptureReport.lossless == true` for every fixture |
| WGT-078 | Derive/retain vibe and shape on import; never overwrite source/user tags | importer + IR | Source tags preserved; derived tags carry confidence; exports report style drift |

### Phase 7 — Hardening and rollout (M7)

| ID | Task | Files / components | Acceptance |
|---|---|---|---|
| WGT-080 | Licence CI gate and `THIRD_PARTY_NOTICES.md` | CI, `third_party/` | Build fails on disallowed licence |
| WGT-081 | Signature verification and supply-chain tests | repo/controller | Tampered pack/index rejected |
| WGT-082 | Fuzz importers/archives/font parsers | CI fuzz targets | No panic/DoS in bounded runs |
| WGT-083 | Sandbox third-party widget code | browser | iframe/WASM sandbox with CSP; no main-origin code |
| WGT-084 | Performance budgets and benchmarks | tests | See §10 |
| WGT-085 | Migration/deprecation: v1 manifests, old packs, dual-read | controller | Existing installs upgrade without data loss |
| WGT-086 | Feature flags + rollout docs | `docs/`, config | Local repo/IR default-on when stable; Connect/importers opt-in |
| WGT-087 | Operator docs: install, sync, create theme, import, font licences | `docs/display-platform/` | Documented end-to-end |

## 8. Data contracts and APIs

### Local controller

```text
GET  /api/v1/asset-db/search?q=&type=
GET  /api/v1/asset-db/entry/{id}
GET  /api/v1/asset-db/versions/{id}
GET  /api/v1/asset-db/categories
POST /api/v1/asset-db/install/{id}?version=
POST /api/v1/asset-db/sync
POST /api/v1/asset-db/submit/{id}
GET  /api/v1/asset-db/stats
POST /api/v1/asset-db/import
```

### Connect

```text
GET  /assets/search
GET  /assets/entry/{id}
GET  /assets/versions/{id}
GET  /assets/categories
POST /assets/submit
GET  /assets/stats
GET  /assets/index/{channel}
```

### Storage

```text
controller/widget_packs/{pack_id}/          # existing installed layout
controller/asset_cache/index.json          # signed index, etag, last sync
controller/asset_cache/packs/{id}/{ver}/   # cached archives/manifests
controller/asset_cache/fonts/{sha256}.ttf  # content-addressed fonts
```

## 9. Test and CI strategy

| Layer | Tests |
|---|---|
| Schemas | JSON Schema fixtures; Rust/Python parity |
| Repository | install/update/rollback/offline; hash/signature/path traversal; yanked |
| IR | source→IR→same-source round-trip; IR→definition snapshot; invalid IR |
| Fonts | metadata/metrics/PANOSE; shape ranking; subsetting; LVGL/u8g2 compile; licence checks |
| Renderers | browser DOM/snapshot; renderer PNG pixel/semantic; LVGL build; AIDA64 SSE/API |
| Importers | golden corpus; lossless capture; export reports |
| Security | fuzz archives/fonts/importers; sandbox escape; CSP; tampered index |
| Performance | budgets in §10; no renderer regression |

Commands will be added per crate/package; examples:

```bash
cargo test -p ozma-display-ir -p ozma-fonts
python3 -m pytest tests/unit/controller/test_asset_repository.py
node --test web/display/**/*.test.mjs
python3 -m pytest tests/unit/connect/test_assets.py
```

## 10. Performance budgets

- Wire definition ≤ 1 MiB (existing limit); full IR/source stored as pack assets.
- Signed index ≤ 2 MiB gzip for 10k packs.
- Install/update a 10 MiB pack ≤ 5 s on a controller.
- Browser first theme render ≤ 250 ms after definition.
- Controller PNG frame ≤ 100 ms for a 1280×720 theme (existing renderer budget).
- Font subset ≤ 2 s for a 300-glyph Latin/CJK subset.
- LVGL font generation ≤ 5 s per theme.
- Offline cache hit renders without network.

## 11. Security and licensing

- Sign index and packs; verify before extraction.
- SHA-256 every file; reject mismatches.
- Safe archive extraction: no absolute paths, `..`, symlinks, hardlinks, device
  files; bounded depth/size/count.
- Executable packs only in iframe sandbox (`sandbox` + CSP) or WASM capability
  boundary.
- No secrets or telemetry in packs; no outbound network without `internet`
  permission.
- Licence allowlist enforced at submit/install; SPDX + `THIRD_PARTY_NOTICES`.
- Font embedding respects `fsType`, OFL RFN, and per-font terms.
- AIDA64/Rainmeter imports are private; submission requires explicit attestation.

## 12. Risks and mitigations

| Risk | Mitigation |
|---|---|
| AIDA64 `.rslcd` format undocumented | Recon spike first; support SensorPanel JSON/known fixtures; fallback to manual mapping |
| Schema churn between Rust/Python/TS | JSON Schema + generated/validated fixtures; M0 gate |
| Font substitution looks wrong | Shape score + metrics + golden corpus + export report; user override |
| Variable/CJK/emoji coverage | Explicit coverage fields; target-specific fallback; no silent drops |
| Pack supply-chain attack | Signatures, hashes, sandbox, fuzz, no code in main origin |
| Repository size/UX | Category packs, installed/manual sync, cache limits |
| Existing packs break | Dual-read v1 manifests; migrate in place; keep `WidgetPackManager` layout |
| Renderer divergence | Shared IR + golden semantic tests, not pixel-only |
| Licence contamination | Allowlist CI, provenance, private imports, no vendor fonts |
| Vibe tags are subjective/culturally biased | Versioned taxonomy, provenance/confidence, human override, advisory-only coherence |
| Derived shape/vibe tags drift on re-import | Preserve original tags; compare derived tags in golden tests; explicit user overrides |

## 13. Rollout

1. **Local-only:** IR + local asset repository + existing packs; no Connect.
2. **Connect opt-in:** shared index/category packs; offline cache.
3. **Fonts:** ship original OFL themes; font substitution reports.
4. **Importers:** private imports; no public submission until licence attestation.
5. **Community:** submission/review workflow, reports, curated categories.
6. **Default-on:** local repo/IR when M0–M3 pass; Connect/importers remain opt-in
   per deployment.

Feature flags:

```text
display.ir.enabled
display.asset_repository.enabled
display.asset_sync.mode=offline|installed|full|manual
display.fonts.translation=enabled
display.importers.enabled
```

## 14. Definition of done

- Existing display adapters and `retro-gauges` still work.
- A lossless source import produces a `SourceCaptureReport` with
  `lossless:true`.
- Every export produces a report with explicit `dropped`/`approximated` fields.
- Repository install/update/rollback works offline and with Connect.
- Font matching and export are deterministic and licence-aware.
- Vibe/shape metadata survives import, repository filtering, and export reports.
- Browser, PNG, LVGL, AIDA64, and Adaptive Cards render the same IR semantics.
- CI gates cover schemas, licences, signatures, fuzz, and performance.
- Docs cover authoring, importing, publishing, and font licences.

## 15. First executable slice

Ship this before broader importers:

1. WGT-001/002/003 schemas and IR crate; Python parity.
2. WGT-004 wire mapping; WGT-005 fixtures.
3. WGT-020–025 local repository + API + existing pack migration.
4. WGT-040–043 IR↔definition and browser/PNG semantic adapters; WGT-046–049
   vibe/shape/coherence.
5. WGT-050–053 font metadata/shape scoring + web/native subset.
6. WGT-060/061/063 browser SVG gauges + controller PNG.
7. WGT-068 `core-dark` theme only.
8. WGT-080/081 licence/signature gates.

That slice proves the architecture without waiting for AIDA64/Rainmeter
reverse-engineering or Connect infrastructure.
