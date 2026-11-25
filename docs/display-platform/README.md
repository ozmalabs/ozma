# Display platform

The display platform turns widgets, themes, fonts, and imported layouts into a
single lossless interchange model so the same intent can render in the browser,
on a controller-rendered PNG, on LVGL panels, in AIDA64 HTML/SSE output, and as
Adaptive Cards.

This directory is the operator and author documentation for the platform.
The authoritative milestone list and task ledger live in
[`../display-platform-execution-plan.md`](../display-platform-execution-plan.md);
the design research and licensing rationale live in
[`../display-widget-library-research.md`](../display-widget-library-research.md).

## Architecture

```text
source formats                     Ozma IR                  targets
──────────────                     ───────                  ───────
AIDA64 / Rainmeter / HA  ─┐
MagicMirror / Grafana    ─┼─> WidgetIR + FontIR + assets ─┬─> browser (SVG/CSS/uPlot)
Adaptive Cards / Ozma    ─┘                               ├─> controller PNG
                                                          ├─> LVGL
                                                          ├─> AIDA64 HTML/SSE
                                                          └─> Adaptive Cards
```

| Component | Location | State |
|---|---|---|
| IR contracts and canonical hashing | `ozma-display-ir/`, `controller/display_ir.py`, `schemas/display/` | Landed |
| Local asset repository | `controller/asset_repository.py` | Landed; configured through `display_config.py` |
| Display feature configuration | `controller/display_config.py` | Landed |
| Licence gate | `scripts/check_display_licenses.py` | Landed |
| Font metadata, matching, and export | `ozma-fonts/`, `controller/font_translation.py`, `controller/font_export.py` | Landed; WOFF2 writing needs optional `brotli` |
| Browser themes and SVG widgets | `web/display/` | Landed: five original themes, gauge/bar/dual/threshold, sparkline/history |
| Render targets | `renderer/display_ir.js`, `controller/lvgl_export.py`, `controller/display_importers/adaptive_cards.py` | Landed: PNG render plan/SVG, LVGL C/JSON, Adaptive Cards; canvas-backed PNG run needs `@napi-rs/canvas` |
| Importers | `controller/display_importers/` | Landed: AIDA64 best-effort, Rainmeter, Home Assistant, MagicMirror, Grafana, Adaptive Cards |
| Connect shared asset database | `connect/assets/` | Landed: models/storage/signed index/API; deployment and key management remain operational work |

Feature status is tracked in the execution plan (§6.1). Treat "partial" as
usable contracts and foundations, not as a finished end-user workflow.

## Feature flags and rollout profile

The display platform is controlled by `OZMA_DISPLAY_PROFILE` plus individual
`OZMA_DISPLAY_*` flags. The controller reads them through
`controller/display_config.py`.

| Profile | IR | Font translation | Local repository | Importers | Sync |
|---|---|---|---|---|---|
| `off` | off | off | off | off | `offline` |
| `local` (default) | on | on | on | off | `offline` |
| `full` | on | on | on | on | `offline` unless explicitly set |

`local` is the shipped default: lossless IR, font translation, and the local
asset repository work out of the box. It does not enable private importers and
does not make network calls. `full` additionally enables importers, but still
leaves sync at `offline`; set `OZMA_DISPLAY_SYNC_MODE` explicitly if you intend
the controller to fetch a remote index. `off` restores the legacy all-off
behaviour.

Explicit variables override the selected profile, so
`OZMA_DISPLAY_PROFILE=local OZMA_DISPLAY_IMPORTERS_ENABLED=1` enables importers
without changing the rest of the local profile. Invalid profile values raise a
typed `DisplayConfigError`.

| Plan flag | Environment variable | Default |
|---|---|---|
| `display.profile` | `OZMA_DISPLAY_PROFILE` (`off`, `local`, `full`) | `local` |
| `display.ir.enabled` | `OZMA_DISPLAY_IR_ENABLED` | on under `local`/`full` |
| `display.asset_repository.enabled` | `OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED` | on under `local`/`full` |
| `display.asset_sync.mode` | `OZMA_DISPLAY_SYNC_MODE` | `offline` for every profile |
| `display.fonts.translation` | `OZMA_DISPLAY_FONTS_TRANSLATION_ENABLED` | on under `local`/`full` |
| `display.importers.enabled` | `OZMA_DISPLAY_IMPORTERS_ENABLED` | on only under `full` |

Related settings:

| Environment variable | Meaning | Default |
|---|---|---|
| `OZMA_DISPLAY_ASSET_INDEX_URL` | Remote index URL for opt-in sync | empty |
| `OZMA_DISPLAY_CACHE_DIR` | Offline cache directory | empty (repository default) |
| `OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS` | Accept unsigned local packs | `false` |
| `OZMA_DISPLAY_SIGNATURE_REQUIRED` | Require signed indexes/packs | `false` |
| `OZMA_DISPLAY_TRUSTED_KEYS` | Trusted signing keys (JSON) | empty |
| `OZMA_DISPLAY_MAX_PACK_BYTES` | Maximum pack payload | 64 MiB |
| `OZMA_DISPLAY_MAX_IMPORT_BYTES` | Maximum import payload | 32 MiB |

Booleans are strict: `1/0`, `true/false`, `yes/no`, and `on/off` are accepted.
Any other value raises a typed `DisplayConfigError` rather than silently
falling back to a default. Sync mode must be one of `offline`, `installed`,
`manual`, or `full`.

**Unsigned and signature policy is unchanged.** A profile never relaxes package
trust. Under `local` or `full`, an unsigned first-party pack still fails unless
you deliberately set `OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS=1` or configure
`OZMA_DISPLAY_TRUSTED_KEYS`/a verifier and sign the pack. The default-on local
repository is not an unsigned-package exception.

**Migrating from all-off.** A deployment that relied on every display feature
being off should set `OZMA_DISPLAY_PROFILE=off`, or set the individual
`OZMA_DISPLAY_*_ENABLED=0` flags explicitly. Explicit flags always win, so old
environments that set individual flags keep the same resolved behaviour for
those flags; only unset flags follow the new `local` default.

## Quick start

1. No exports are required for the safe default. To pin the legacy all-off
   behaviour, or to opt into importers without network sync, choose a profile:

   ```bash
   # Legacy all-off behaviour.
   export OZMA_DISPLAY_PROFILE=off

   # Local default (also the implicit default).
   export OZMA_DISPLAY_PROFILE=local

   # Importers on, network sync still explicitly offline.
   export OZMA_DISPLAY_PROFILE=full
   export OZMA_DISPLAY_SYNC_MODE=offline
   ```

2. Inspect the resolved configuration:

   ```bash
   PYTHONPATH=controller python - <<'PY'
   from display_config import DisplayConfig

   config = DisplayConfig.from_env()
   print(config.to_dict())
   print(config.feature_flags())
   PY
   ```

3. Run the licence gate against the checkout:

   ```bash
   python scripts/check_display_licenses.py --root . --mode local
   ```

   Published/community validation uses `--mode published`, which rejects the
   first-party AGPL pack that local mode intentionally permits.

4. Run the focused tests:

   ```bash
   python -m pytest \
     tests/unit/controller/test_asset_repository.py \
     tests/unit/controller/test_display_ir.py \
     tests/unit/controller/test_display_config.py \
     tests/unit/controller/test_display_license_gate.py -q
   ```

## Documentation map

- [Authoring](authoring.md): Widget IR, theme tokens, design language,
  accessibility, and size budgets.
- [Importing](importing.md): supported formats, lossless capture, private
  imports, and retention of unknown/script content.
- [Publishing](publishing.md): manifest fields, licence allowlist, provenance,
  signing, yanking, and review.
- [Fonts](fonts.md): font intent and shape matching, embedding licences,
  substitution reports, and export targets.
- [Operations](operations.md): sync modes, offline cache, install/update/
  rollback, asset-db endpoints, and troubleshooting.
- [Release notes](release-notes.md): M0–M7 status, feature flags, migration
  and upgrade steps, verification commands, and known caveats.

## Security posture

- Pack code is never imported or executed by the repository or the licence
  gate.
- Every declared asset hash is verified before install; the licence gate
  re-verifies hashes that are present in the checkout.
- Executable pack assets require a sandbox declaration.
- Archives are read member-by-member with path, depth, count, and size limits.
- Third-party widget code is expected to run only inside an iframe/WASM
  boundary; the browser sandbox work is tracked under M7 and is not complete.
