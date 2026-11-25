# Operating the display asset repository

This guide covers the local controller repository: sync modes, the offline
cache, install/update/rollback, the asset-db API, private imports, and common
failures. A local repository works without Connect; the optional Connect
service exposes a shared, signed index for multi-controller deployments.

Configuration comes from `controller/display_config.py`. The repository itself
is in `controller/asset_repository.py`, and the HTTP router is wired into the
controller API.

## Sync modes

`OZMA_DISPLAY_SYNC_MODE` (or `AssetSyncPolicy.mode`) selects how much metadata
the controller keeps:

| Mode | Network | Behaviour |
|---|---|---|
| `offline` | Never | Use the cached/local index only. Sync returns the cache without a request. |
| `installed` | Opt-in | Sync the index but keep only entries for already-installed packs. |
| `manual` | Opt-in | Sync on an explicit operator action; no automatic polling. |
| `full` | Opt-in | Sync the full index, subject to cache and policy limits. |

`offline` is the default. A mode other than `offline` still falls back to the
cache when a sync fails; the controller does not lose its last known index
because the network is unavailable. The policy also controls
`sync_interval_hours`, `cache_size_mb`, `include_previews`,
`include_external_imports`, and `verify_signatures`.

Imported packs are excluded unless `include_external_imports` is enabled. This
keeps private imports from appearing in shared listings.

## Offline cache

The repository keeps its state beside the installed packs:

```text
controller/widget_packs/{pack_id}/          # live installed copy
controller/asset_cache/index.json           # signed index, ETag, last sync
controller/asset_cache/installed.json       # current/previous versions
controller/asset_cache/submissions.json     # local submission queue
controller/asset_cache/packs/{id}/{ver}/    # immutable cached snapshots
controller/asset_cache/fonts/{sha256}.ttf   # content-addressed fonts
```

The index is written atomically. A downloaded index replaces the local one only
after JSON parsing and any required signature verification succeed, so a
truncated or tampered response cannot corrupt the cache.

An offline cache hit renders without network access. Use `offline` mode when the
deployment must make no outbound calls at all.

## Install, update, and rollback

- **Install** verifies licence policy (when enabled), signatures (when
  required), every declared hash, sizes, and undeclared payload files before
  writing. Extraction is atomic: a failed install leaves the previous version
  in place.
- **Update** follows the same checks and moves the current version to the
  previous slot so it can be restored.
- **Rollback** restores the previous installed version from the cache. It does
  not need the network.
- **Yanked** versions are never selected automatically. An explicit install of
  a known yanked version is still possible for audit or recovery, but it is not
  an automatic update target.
- **Rollback history** is intentionally small: current and previous are kept per
  pack. Keep external backups if you need longer retention.

The repository never imports or executes pack code. Installing a pack writes
files; it does not activate arbitrary Python or JavaScript in the controller
process.

## Asset-db endpoints

Local controller routes from the plan:

| Method and path | Purpose | State |
|---|---|---|
| `GET /api/v1/asset-db/search?q=&type=` | Search local/cached index | Wired |
| `GET /api/v1/asset-db/entry/{id}` | Pack metadata | Wired |
| `GET /api/v1/asset-db/versions/{id}` | Version history | Wired |
| `GET /api/v1/asset-db/categories` | Category counts | Wired |
| `POST /api/v1/asset-db/install/{id}?version=` | Install a version | Wired |
| `POST /api/v1/asset-db/sync` | Sync index or return cache | Wired |
| `POST /api/v1/asset-db/submit/{id}` | Queue a local submission | Wired locally |
| `GET /api/v1/asset-db/stats` | Index/cache statistics | Wired |
| `POST /api/v1/asset-db/import` | Import a private asset | Use the dedicated display import API below |
| `POST /api/v1/display/import` | Import and store a private source | Wired |
| `GET /api/v1/display/importers` | List registered importers | Wired |
| `GET /api/v1/display/imports` | List private imports | Wired |
| `GET /api/v1/display/imports/{id}` | Read an import manifest | Wired |
| `GET /api/v1/display/imports/{id}/raw` | Fetch the retained raw source | Wired |
| `DELETE /api/v1/display/imports/{id}` | Delete a private import | Wired |

Connect routes (`/assets/search`, `/assets/entry/{id}`,
`/assets/versions/{id}`, `/assets/categories`, `/assets/submit`,
`/assets/stats`, `/assets/index/{channel}`) are served by `connect/assets/`.
The controller verifies the signed index and replaces its cache only after
canonical-JSON signature and ETag checks succeed. See [connect.md](connect.md)
for deploying that service: environment variables, storage layout, SQLite vs
Postgres, key generation, TLS, and backup/restore.

## Configuration example

```bash
export OZMA_DISPLAY_ASSET_REPOSITORY_ENABLED=1
export OZMA_DISPLAY_SYNC_MODE=installed
export OZMA_DISPLAY_ASSET_INDEX_URL=https://assets.example.test/index.json
export OZMA_DISPLAY_CACHE_DIR=/var/lib/ozma/asset-cache
export OZMA_DISPLAY_ALLOW_UNSIGNED_PACKS=0
export OZMA_DISPLAY_MAX_PACK_BYTES=67108864
export OZMA_DISPLAY_MAX_IMPORT_BYTES=33554432
```

Resolved values can be inspected without touching the network:

```bash
PYTHONPATH=controller python - <<'PY'
from display_config import DisplayConfig

config = DisplayConfig.from_env()
print(config.to_dict())
print(config.feature_flags())
PY
```

If the controller constructs `AssetRepository` directly, pass the resolved
config values through (`policy`, `allow_unsigned`, `require_signature`,
`limits`, and `index_url`). The dataclass defaults are the authoritative
deployment defaults; repository constructor defaults may be more permissive for
backwards compatibility.

## Verifying a checkout

Run the licence/hash gate before installing or publishing:

```bash
python scripts/check_display_licenses.py --root . --mode local
python scripts/check_display_licenses.py --root . --mode published --json
```

Local mode permits AGPL only for the configured first-party author. Published
mode uses the community allowlist. The gate reads manifests, verifies hashes for
files that exist, reports missing declared files, and rejects executable assets
without a sandbox. It exits non-zero on errors and never executes pack code.

## Migrating manifests for a release

Older pack manifests can be upgraded in place with the migration script. It
dry-runs by default, preserves unknown fields and option values, computes
hashes only for files that exist, rejects symlinks and traversal, and refuses
to overwrite a newer or invalid manifest without `--force`:

```bash
# Report what would change; writes nothing.
python scripts/migrate_display_packs.py --root controller/widget_packs

# Apply atomically, keeping a .manifest.json.bak beside each manifest.
python scripts/migrate_display_packs.py \
  --root controller/widget_packs --write --backup

# Re-run to confirm the migration is idempotent (changed=0).
python scripts/migrate_display_packs.py \
  --root controller/widget_packs --dry-run
```

The JSON report is `{"checked", "changed", "skipped", "errors"}`. Manifests
that already carry the extended lifecycle block are skipped byte-identically;
use `--force` only to re-normalize them deliberately. See the
[release notes](release-notes.md) for upgrade steps, rollback, feature flags,
and known caveats, and [publishing](publishing.md) for the manifest contract.

## Troubleshooting

| Symptom | Likely cause | Action |
|---|---|---|
| `unknown sync mode` | Bad `OZMA_DISPLAY_SYNC_MODE` | Use `offline`, `installed`, `manual`, or `full` |
| `has no SHA-256` / `SHA-256 mismatch` | Manifest or payload changed | Recompute the hash from the intended file; do not install until they match |
| `declared asset missing` | Manifest references a file not shipped | Add the file or remove the declaration |
| `executable asset ... has no sandbox` | Script/plugin declared without a boundary | Declare `iframe`/WASM sandbox or remove executable content |
| `unsigned packs are not allowed` | Policy requires signatures | Configure a verifier and sign the pack/index, or relax the local policy deliberately |
| `signature verification failed` | Wrong key, wrong payload, or tampering | Reject the pack; investigate the source |
| `licence ... is not on the publish allowlist` | Disallowed or unknown licence | Fix the licence metadata or do not publish |
| `pack exceeds total size limit` | Payload larger than policy | Reduce assets or raise the limit deliberately |
| `path traversal` / `symlink` / `hardlink` | Unsafe archive | Reject the archive; rebuild it with regular files and relative paths |
| Offline render fails after a sync | Cache/index not present or corrupt | Restore `controller/asset_cache/` from backup; `offline` mode will not fetch it |
| Old pack disappeared after update | New version replaced current | Roll back to the previous cached version |
| Import does not appear in search | Imports private by default | Enable `include_external_imports` only if policy permits |

For supply-chain incidents, yank the affected version, preserve logs and
hashes, rotate signing keys if needed, and publish a replacement version.
Yanking hides a version from automatic resolution but is not a substitute for
removing illegal or malicious objects.

See the [execution plan](../display-platform-execution-plan.md) §7 (M1/M2/M7)
and §8 for the storage/API contract, and the
[research document](../display-widget-library-research.md) for the repository
design and sync precedent.
