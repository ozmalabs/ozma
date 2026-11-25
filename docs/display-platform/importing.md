# Importing layouts and skins

Importing is the process of turning a foreign widget, panel, or theme into
Ozma artifacts without discarding source information. The invariant is simple:

> Import is lossless. Export is the only loss boundary.

The importer workstream (M6) is implemented for the formats below. Importers
are opt-in (`OZMA_DISPLAY_IMPORTERS_ENABLED=false` by default) and never execute
untrusted source code. Parsing is best-effort; the retained raw source remains
authoritative when a format is partially understood.

## Supported formats

| Format | Direction | State | Notes |
|---|---|---|---|
| Ozma `DisplayDefinition` / `ScreenLayout` | import and export | Supported | Existing packs continue to load unchanged |
| Ozma `WidgetIR` | import and export | Supported | Lossless round-trip is the reference case |
| Adaptive Cards JSON | import and export | Supported subset | Unsupported elements retained; export report lists drops |
| Grafana panel/dashboard JSON | import | Supported | Charts/gauges/bars/stat mapped; queries retained opaque |
| Home Assistant card YAML | import | Supported | Entities become bindings; custom cards retained opaque |
| MagicMirror configuration | import | Supported (non-evaluating) | Known modules mapped; JS/CSS retained opaque |
| Rainmeter `.ini` skins | import | Supported | Raw INI, assets, and unsupported meters retained |
| AIDA64 `.rslcd` / SensorPanel | import | Best-effort | ZIP/XML/JSON/HTML are read; unknown payloads stay opaque |

AIDA64 and SensorPanel support means **importing a user's own files as data**.
Ozma does not bundle, redistribute, or ship AIDA64 application files, official
designs, forum panels, or paid skins. Rainmeter skins have per-skin licences and
are treated the same way: import privately, never republish without explicit
permission.

## Lossless capture

Every import produces a `SourceCaptureReport`. A successful import must report
`lossless: true`; that means:

- the raw source document is retained byte-for-byte;
- unknown fields, extensions, and format-specific blocks are retained;
- referenced images, fonts, and other assets are retained as pack assets;
- scripts and executable content are retained as opaque files, not parsed or
  run;
- the parsed IR is stored alongside the source, not instead of it;
- derived tags carry provenance and confidence and do not overwrite source or
  user tags.

If a format cannot be captured losslessly, the import must fail or be marked
explicitly lossy with a report. Silent dropping is not acceptable.

Translations to a target are separate from the import. Each export emits a
`TranslationReport` (and a `FontTranslationReport` where fonts change) listing
what was preserved, approximated, dropped, or substituted.

## Private imports by default

Imported content is private to the installation:

- an import is never auto-published;
- submission to a shared index requires an explicit user attestation that the
  user has the right to share the material;
- imported packs are excluded from sync results unless the sync policy
  explicitly includes external imports
  (`include_external_imports`, default `false`);
- provenance records where the source came from and how it was obtained.

The reserved local endpoint is `POST /api/v1/asset-db/import`. It is part of the
data-contract plan, not a live importer endpoint yet; M6 must land before
clients can depend on it.

## Unknown and executable content

Unknown content is valuable evidence about the source, so it is retained, but
retained content is not trusted.

- Unknown JSON/YAML blocks are preserved under the source document.
- Unknown binary assets are stored by hash with their declared media type.
- Scripts, plugins, and executable assets are stored as opaque files. They are
  never imported, evaluated, or executed by the controller, the repository, or
  the licence gate.
- Any executable asset that is later exposed to a renderer must declare a
  sandbox (`iframe` or a WASM capability boundary) in its manifest. The licence
  gate rejects executable assets without one.
- Archive extraction rejects absolute paths, `..`, symlinks, hardlinks, device
  files, duplicate paths, excessive depth/count, and oversize payloads.
- Imports are bounded by `OZMA_DISPLAY_MAX_IMPORT_BYTES` (32 MiB default) in
  addition to the repository's per-file and total limits.

Retention does not imply permission. A retained script or proprietary asset
does not become redistributable because it survived the import.

## Import workflow

1. Capture the raw source and compute hashes for every captured file.
2. Parse into IR where a parser exists; keep the full source regardless.
3. Record provenance, unknown fields, assets, and opaque scripts.
4. Derive vibe/shape tags with confidence; leave source/user tags untouched.
5. Validate the IR and produce a `SourceCaptureReport` with `lossless: true`.
6. Store the import privately.
7. When exporting to a target, generate the translation and font reports.

## Fonts during import

Imported fonts follow the font policy in [fonts.md](fonts.md). A font is
captured for fidelity, but embedding is a separate decision controlled by
`fsType`, the OFL Reserved Font Name, and the destination. If a font cannot be
embedded, the export substitutes a licensed font and records the substitution
in the font report.

## Review checklist

- [ ] Raw source is present and hashed.
- [ ] Unknown fields and assets survived.
- [ ] Scripts are opaque and sandbox policy is declared where needed.
- [ ] `SourceCaptureReport.lossless` is true, or the loss is explicit.
- [ ] Provenance is recorded.
- [ ] The import is private and not queued for publication.
- [ ] AIDA64/Rainmeter material is not redistributed.

See the [execution plan](../display-platform-execution-plan.md) §7 (M6) for the
importer backlog and the
[research document](../display-widget-library-research.md) for the licensing
analysis of source ecosystems.
