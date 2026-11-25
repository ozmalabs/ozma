# Display platform release notes

**Status:** M0–M6 landed; M7 hardening is partially landed. This document is
the release-facing summary for the display platform work tracked in
[`../display-platform-execution-plan.md`](../display-platform-execution-plan.md)
(§6.1, §7, and §13). The authoritative task ledger remains the execution plan.

## Milestone status

| Milestone | State | Release summary |
|---|---|---|
| M0 — Contracts and fixtures | **Done** | Lossless Widget IR, canonical hashing, JSON Schemas, and golden fixtures are shared by Rust and Python. |
| M1 — Local asset repository | **Done** | Local index/cache, install/update/rollback, hash and signature verification, safe archive handling, offline sync policy, and the asset-db router. `retro-gauges` loads unchanged. |
| M2 — Connect shared database | **Done** | Postgres/SQLite models and migration, object storage, deterministic signed index, full search/submit/stats API, licence and attestation rules, environment bootstrap for deployment, controller sync integration. |
| M3 — IR and translation core | **Done** | IR ↔ `ScreenLayout`/`DisplayDefinition`, semantic adapters, SVG previews, LVGL export/reports, PNG render planning, and a fake-canvas-tested canvas path. Real raster output needs the optional `@napi-rs/canvas` dependency. |
| M3b — Vibe/shape/coherence | **Done** | Versioned taxonomy, shape extraction, source/user/derived keyword separation, advisory coherence reports, repository filtering, Rust/Python parity. |
| M4 — Font subsystem | **Done** | Font metadata, shape/intent matching, TTF/OTF subsetting, BDF/LVGL export, previews, `fsType`/OFL licence checks, and a Latin/CJK/Arabic/Indic/icon golden corpus. WOFF2 writing needs optional `brotli`. |
| M5 — Render adapters and themes | **Done** | Five original themes, SVG gauge/bar/dual/threshold, VU meters with attack/release, dB/s falloff, peak hold, clip latch, RMS and stereo, sparkline/history widgets, sandboxed third-party widgets, PNG render plans, LVGL C/JSON, Adaptive Cards export, and AIDA64 `/panel` generated from `WidgetIR`. Optional editor only. |
| M6 — Importers | **Done** | AIDA64/SensorPanel, Rainmeter, Home Assistant, MagicMirror, Grafana, and Adaptive Cards import/export with lossless capture and private-by-default storage. |
| M7 — Hardening and rollout | **Done (except fuzz)** | Licence/checksum/sandbox gate, Ed25519 trust store and signing CLI, browser sandbox/CSP, migration tooling, performance benchmarks, targeted CI, environment bootstrap for Connect, operator docs, and these release notes. Default-on rollout and marketplace/review UI remain product decisions. |

Treat “mostly done” as usable contracts and foundations, not a finished
end-user workflow. See the execution plan §6.1 for the live gap list.

## Feature flags

All display features remain opt-in in this release. Flags are read by
`controller/display_config.py`.

| Plan flag | Environment variable | Default |
|---|---|---|
| `display.ir.enabled` | `OZMA_DISPLAY_IR_ENABLED` | `false` |
| `display.asset_repository.enabled` | `OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED` | `false` |
| `display.asset_sync.mode` | `OZMA_DISPLAY_SYNC_MODE` (`offline`, `installed`, `manual`, `full`) | `offline` |
| `display.fonts.translation` | `OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED` | `false` |
| `display.importers.enabled` | `OZMA_DISPLAY_IMPORTERS_ENABLED` | `false` |

Related settings:

| Environment variable | Meaning | Default |
|---|---|---|
| `OZMA_DISPLAY_ASSET_INDEX_URL` | Remote signed index URL for opt-in sync | empty |
| `OZMA_DISPLAY_CACHE_DIR` | Offline cache directory | empty (repository default) |
| `OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS` | Accept unsigned local packs | `false` |
| `OZMA_DISPLAY_SIGNATURE_REQUIRED` | Require signed indexes/packs | `false` |
| `OZMA_DISPLAY_TRUSTED_KEYS` | Trusted signing keys (JSON) | empty |
| `OZMA_DISPLAY_MAX_PACK_BYTES` | Maximum pack payload | 64 MiB |
| `OZMA_DISPLAY_MAX_IMPORT_BYTES` | Maximum import payload | 32 MiB |

Booleans are strict (`1/0`, `true/false`, `yes/no`, `on/off`); invalid values
raise `DisplayConfigError` instead of falling back silently. The plan’s
“default-on” milestone has not been reached: local repository and IR remain
opt-in, and Connect/importers are opt-in per deployment.

## Behavioural and compatibility notes

- **Wire compatibility is unchanged.** Existing `DisplayDefinition`,
  `ScreenLayout`, and installed widgets continue to load. The IR is stored as a
  compact projection where required; raw source and unknown fields stay in the
  pack.
- **Manifest defaults are applied on read.** Missing `type` and `schema` default
  to `widget_pack` and `org.ozma.asset-pack/1` in
  `AssetPackEntry.from_manifest` and the widget-pack loader.
- **Migration rewrites formatting when it changes a manifest.** Migrated
  manifests are canonical JSON (sorted keys, two-space indent, trailing
  newline). Expect key-order-only diffs in the first migration commit.
- **Unknown fields and option values are preserved.** Migration does not drop
  renderer extensions, options, tags, themes, or custom blocks. Do not use that
  tolerance to smuggle renderer state; namespace extensions and document them.
- **Already-extended manifests are skipped.** A manifest that already carries
  licence, provenance, confidence, capabilities, permissions, assets, and
  checksums is left byte-identical. Use `--force` only when you deliberately
  want to re-normalize it.
- **Hashes are computed only for files that exist.** Declarations for missing
  files are retained so the licence gate can report them. Symlinks, absolute
  paths, drive-qualified paths, backslashes, and `..` components are rejected;
  pack code is never imported or executed.
- **Local installs are more permissive than publication.** Local mode permits
  first-party AGPL (`retro-gauges`); published mode enforces the community
  allowlist. Executable assets require a sandbox declaration.
- **Offline is the default sync mode.** Enabling any other mode opts the
  deployment into index network access. Sync failures fall back to the last
  cached index.
- **Importers remain private-by-default.** Imported material is not published
  automatically and requires explicit attestation before submission.
- **Connect is a service, not a bundled deployment.** Enabling shared assets
  requires a running Connect instance, object storage, and key management.

## Upgrade and migration

No database or wire migration is required for existing controllers. For a
checkout that still has pre-extension manifests:

```bash
# 1. Inspect what would change. This is the default and writes nothing.
python scripts/migrate_display_packs.py --root controller/widget_packs

# 2. Apply with a backup beside each manifest.
python scripts/migrate_display_packs.py \
  --root controller/widget_packs --write --backup

# 3. Verify. The second run must report changed=0 and produce no diff.
python scripts/migrate_display_packs.py \
  --root controller/widget_packs --dry-run
python scripts/check_display_licenses.py --root . --mode local
```

The JSON report has the shape
`{"checked": N, "changed": N, "skipped": N, "errors": [...]}` and exits
non-zero on any error. `--backup` writes `.manifest.json.bak` before the atomic
replace. To roll back a pack, restore its `.manifest.json.bak` and reinstall or
reload the pack. A manifest is not overwritten if it changed while the script
was running; re-run after reviewing the diff.

Manifests with a schema other than `org.ozma.asset-pack/1` are refused unless
`--force` is passed. Malformed JSON fails closed unless `--force` is passed;
the forced path infers the pack id from the directory name and writes a minimal
v1 manifest, so prefer restoring valid metadata from version control.

Existing installs upgrade in place: `WidgetPackManager` keeps the
`controller/widget_packs/{id}/` layout, and the repository keeps its cache under
`controller/asset_cache/`. Rollback is per pack (current and previous version)
and does not require the network.

## Verification commands

Run the focused gates locally:

```bash
python -m pytest tests/unit/controller/test_migrate_display_packs.py -q
ruff check scripts/migrate_display_packs.py \
  tests/unit/controller/test_migrate_display_packs.py
python -m py_compile scripts/migrate_display_packs.py
python scripts/migrate_display_packs.py \
  --root controller/widget_packs --dry-run
python scripts/check_display_licenses.py --root . --mode local
```

Published-mode validation is expected to fail for the first-party AGPL packs:

```bash
python scripts/check_display_licenses.py \
  --root . --mode published --json
```

Broader platform checks:

```bash
cargo test -p ozma-display-ir -p ozma-fonts
cargo clippy -p ozma-display-ir -p ozma-fonts --all-targets --no-deps -- -D warnings
cargo fmt -p ozma-display-ir -p ozma-fonts -- --check
node --test web/display/*.test.mjs web/display/widgets/*.test.mjs \
  renderer/display_ir.test.mjs
```

The targeted CI workflow is `.github/workflows/display-platform.yml`. It runs
the Python migration/licence gates, the Rust IR/font checks, and the Node
syntax/tests with `permissions: contents: read`; it has no secrets and no
network services.

## Known caveats

- **AIDA64 is best-effort.** Native `.rslcd` is undocumented and opaque by
  design; the importer supports known ZIP/XML/JSON/HTML shapes and retains raw
  source. AIDA64 panel output from IR is not part of this release.
- **Canvas-backed PNG rendering is optional.** The render-plan adapter is
  landed, but an end-to-end canvas run requires `@napi-rs/canvas`; without it,
  the canvas path is not exercised in this checkout.
- **WOFF2 writing needs optional `brotli`.** Without it, TTF/OTF subsetting and
  other export targets still work; WOFF2 write tests are skipped.
- **Connect deployment is operational work.** Models, storage, signed index,
  and APIs are implemented, but there is no bundled production deployment,
  review UI, or key-distribution process. Use Ed25519 keys in production and
  distribute public keys out of band.
- **Browser sandbox/CSP enforcement is incomplete.** Third-party executable
  widget code must not run in the main origin. The iframe/WASM boundary and CSP
  hardening remain open under M7; the repository and licence gate never execute
  pack code.
- **Golden visual parity and performance budgets are not CI gates yet.**
  Browser/AIDA64/LVGL/Adaptive Cards pixel and semantic parity snapshots, fuzz
  targets, and the §10 budgets remain follow-up work.
- **Signature verification is policy-driven.** The licence gate checks licences,
  hashes, and sandbox declarations only. Deployments that require signatures
  must configure a verifier; a signed manifest without a verifier fails closed.
- **The migration script is metadata-only.** It does not rewrite pack payloads,
  sign manifests, publish to Connect, or validate renderer semantics.

## Related documentation

- [Operations](operations.md): sync, cache, install/update/rollback, API.
- [Publishing](publishing.md): manifest fields, licence allowlist, provenance,
  signing, yanking.
- [Authoring](authoring.md): Widget IR, themes, accessibility, size budgets.
- [Importing](importing.md): supported formats and private imports.
- [Fonts](fonts.md): font intent, embedding licences, substitution reports.
- [Execution plan](../display-platform-execution-plan.md): milestone ledger and
  remaining tasks (`WGT-080`–`WGT-087`).
