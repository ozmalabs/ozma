# Phase 4 — local Restic backup report

Status: **implemented (2026-11-25).** `ozma-node` now has an opt-in local Restic
provider at `/backup/status`, `/backup/init`, `/backup/run`,
`/backup/snapshots`, `/backup/restore`, and
`DELETE /backup/snapshots/{id}`. It replaces only the core of the previous
typed `/backup/*` 501 stub; app inventory, cloud destinations, config writes,
estimate/verify/onboarding, scheduling, DB hooks, and retention policy remain
typed 501s.

`restic` 0.20.0 (Go 1.27.4, linux/amd64) was installed on the test host. The
implementation uses the CLI directly and has no Rust Restic dependency.

## Design

### Opt-in flags

| Flag | Rule |
|---|---|
| `--backup-repo PATH` | Absolute local path only. `s3:`, `sftp:`, `rest:`, `http(s):`, `azure:`, `gs:`, `b2:`, `rclone:`, and `swift:` prefixes are rejected. Exact system roots (`/`, `/etc`, `/home`, `/var`, `/tmp`, …) are rejected. |
| `--backup-password-file PATH` | Absolute regular file, non-empty, at most 4 KiB, mode `0600`, not a symlink. |
| `--backup-source PATH` | Repeatable; the complete source allow-list. Default none. Each root must exist, be a directory, and not be an exact whole-system root. |
| `--backup-restore-dir PATH` | Repeatable; the complete restore-target allow-list. Default none, so restore is unavailable until an operator adds one. |
| `--backup-restic PATH` | Optional executable override; default resolves `restic` on `PATH`. |

`--backup-source` and `--backup-restore-dir` cannot be supplied without
`--backup-repo`. Invalid configuration exits at startup with `code`, error, and
remediation instead of silently starting a half-configured provider.

### Security model

* **No shell.** Every operation starts the resolved restic executable directly
  with an exact argv. Paths are never interpolated into a shell string.
* **Password isolation.** The password file contents are never read by
  `ozma-node`. Restic receives only `RESTIC_PASSWORD_FILE`; the password is
  never in argv, logs, or status JSON. The status document says
  `password.configured:true` and does not expose the path or contents.
* **Environment isolation.** Children run with a cleared environment containing
  only `RESTIC_REPOSITORY`, `RESTIC_PASSWORD_FILE`, `RESTIC_PROGRESS_FPS=0`, a
  minimal `PATH`, and `LANG`/`LC_ALL=C`. Parent `RESTIC_*`/cloud credentials do
  not leak into a backup child.
* **Source allow-list.** `POST /backup/run` defaults to the configured sources
  and can select at most 16 requested paths. Each requested path is
  canonicalized and must equal or descend from a configured source. Relative
  paths, `..`, NUL/newline, missing paths, and symlinks that resolve outside the
  allow-list are rejected.
* **Restore confinement.** A restore target must be below one configured
  restore root, must not equal the root itself, and must not descend from the
  repository. Paths are lexically normalized, every existing component is
  checked, symlink components are rejected, missing directories are created one
  component at a time, and the final canonical target must still be inside the
  root.
* **No remote repositories.** Only local filesystem repositories are accepted;
  cloud credentials are neither read nor stored.
* **No home/system defaults.** Unlike the Python agent, there are no default
  include paths. The operator must name each source.
* **Bounded work.** Maximum 16 sources, 8 restore roots, 16 run paths, 8 tags,
  64-character tags, 4096-character paths, 4 MiB stdout, 8 KiB stderr, and
  100 snapshots per list. Each operation has a timeout; stdout/stderr are read
  with hard caps; `kill_on_drop` stops a timed-out child.
* **One mutating operation at a time.** A concurrent init/run/restore/delete
  returns 409 `backup.busy`; read-only status/snapshot polls do not hold the
  operation lock.

### Exact restic argv

```
restic --no-cache init
restic --no-cache backup --json [--tag TAG]... PATH...
restic --no-cache snapshots --json
restic --no-cache restore SNAPSHOT --target TARGET [--include SOURCE_PATH]
restic --no-cache forget SNAPSHOT_ID --prune
```

`--include` is omitted when the snapshot source path is `/` (whole-snapshot
restore). Restore preserves absolute snapshot paths below `--target`, matching
restic semantics.

## Endpoint behaviour

### `GET /backup/status`

Without `--backup-repo`, returns 200 with `configured:false`,
`repo.state:"unconfigured"`, `restic.available:false`, empty source/restore
lists, and `last_snapshot:null`. With a repository, returns 200 with:

* `repo.state`: `uninitialized` until `<repo>/config` exists, then `ready`.
* `restic.available` and the resolved program path.
* `sources` and `restore_roots` (explicit allow-lists).
* `last_snapshot`: the newest snapshot, if restic snapshots succeeds.
* `snapshot_error`: a stable code when the repository is ready but listing
  fails.
* `last_run`: timestamps plus the last typed error code (never a raw stderr or
  secret).

A missing binary or bad password-file permissions is a typed 503/403 status
error, not a fabricated `ready`.

### `POST /backup/init`

Idempotent. An existing `<repo>/config` returns `already:true` without running
restic. A missing repository runs `restic --no-cache init`; a restic
"already initialized" response is also treated as `already:true`. Two
concurrent init requests cannot both run.

### `POST /backup/run`

Request:

```json
{"mode": "smart", "paths": ["/srv/data/docs"], "tags": ["manual"]}
```

`mode` accepts `smart`, `files`, or `manual`; all mean configured-source
selection in this provider. `disk_image` and `advanced` return 501
`backup.mode-unsupported` rather than pretending app/DB/disk-image work ran.
With no `paths`, the configured sources are used. The response contains
`ok:true`, echoed mode/selection/paths/tags, the restic summary
(`files_new`, `files_changed`, `total_files_processed`,
`total_bytes_processed`, `data_added`, `snapshot_id`, …), and an explicit
`limitations` object (`app_inventory:false`, `db_hooks:false`,
`cloud_destination:false`, `disk_image:false`).

### `GET /backup/snapshots?limit=N`

Default 20, clamped to 1–100. Returns newest-first, bounded snapshots with only
`id`, `short_id`, `time`, `hostname`, `tags`, `paths`, and
`program_version`; `total` and `truncated` describe the full list. No password,
environment, repository credential, or raw restic stderr is included.

### `POST /backup/restore`

Request:

```json
{"snapshot_id": "0123…cdef", "source_path": "/srv/data", "target_path": "/srv/restore/run-1"}
```

`latest` is accepted for restore; otherwise the id is 8–64 hex characters.
`target_path` is required and must be below a configured restore root.
`source_path` must be an absolute path within the snapshot (no `..` or control
characters). A successful response reports the snapshot, source, target, and
`restored:true`; there is no "partial success" shape.

### `DELETE /backup/snapshots/{id}`

Deletes one exact snapshot. `latest` is rejected, the id must be 8–64 hex
characters, and the id must resolve to exactly one snapshot. The argv is
`forget <full-id> --prune`: only that snapshot is forgotten and only
unreferenced repository data is pruned. No `--keep-*` policy or unrelated
retention rule is applied. Response: `deleted`, `pruned:true`,
`policy:"exact-snapshot-only"`.

### Unsupported sub-surfaces

`/backup/apps`, `/backup/apps/restore`, `/backup/config`, `/backup/estimate`,
`/backup/verify`, `/backup/onboarding`, snapshot file browsing, and any cloud
destination remain HTTP 501 with the stable `backup.unsupported` document.
`GET /api/v1/connection.controller_surfaces` advertises the six implemented
core capabilities as `supported:true` and the remaining `/backup/*` surface as
`supported:false`.

## Typed errors

All errors have `ok:false`, `supported:true`, a stable `code`, `error`, and
`remediation`; HTTP statuses are typed.

| Code | Status | Cause |
|---|---|---|
| `backup.restic-missing` | 503 | Binary cannot be resolved/executed. |
| `backup.not-configured` | 503 | Core operation without `--backup-repo`. |
| `backup.repo-invalid` | 400 | Remote/relative/denied repository path. |
| `backup.password-file` | 400 | Missing, symlinked, non-regular, empty, or oversized file. |
| `backup.password-permissions` | 403 | Mode has group/world bits. |
| `backup.password-rejected` | 502 | Restic wrong/invalid password. |
| `backup.repo-uninitialized` | 409 | Run/restore/snapshots/delete before init. |
| `backup.permission` | 403 | Filesystem permission failure. |
| `backup.no-sources` | 409 | No allow-listed source. |
| `backup.path-not-allowed` | 400 | Run path outside the source allow-list. |
| `backup.restore-not-configured` | 503 | Restore requested without a restore root. |
| `backup.restore-target-required` | 400 | Missing target. |
| `backup.restore-path-denied` | 403 | Target outside a root, root itself, symlink, or repo path. |
| `backup.snapshot-invalid` | 400 | Bad id, `latest` for delete, or ambiguous prefix. |
| `backup.snapshot-not-found` | 404 | No matching snapshot. |
| `backup.tag-invalid` | 400 | Bad tag characters/length/count. |
| `backup.mode-unsupported` | 501 | `disk_image`/`advanced`/unknown mode. |
| `backup.busy` | 409 | Another mutating operation is running. |
| `backup.restic-failed` | 502 | Other restic non-zero exit (sanitized detail). |
| `backup.timeout` | 504 | Operation timeout. |
| `backup.io` | 500 | Local process/IO failure. |

## Controller proxy

`controller/api.py` adds `_proxy_backup_surface`, which:

* returns a successful node JSON document verbatim (snapshots are not wrapped
  twice);
* preserves any `supported:false` document or `backup.*` error with its node
  status, code, error, and remediation;
* forwards the per-node bearer token only in the request `Authorization`
  header;
* never copies the token into a response body or error detail.

The controller exposes the same core through
`GET /api/v1/backup/nodes/{id}/status`, `POST .../init`, `GET .../snapshots`,
`DELETE .../snapshots/{snapshot_id}`, `POST .../restore`, and `POST .../run`.
`POST .../run` forwards bounded `paths`/`tags` and defaults `mode` to `smart`.
`_proxy_node_response` now sends the actual HTTP method (`DELETE` included)
instead of always issuing a GET for non-POST methods.

## Tests and live evidence

Rust (`cargo test -p ozma-node --lib backup::`):

* `run_uses_exact_argv_and_never_puts_the_password_in_argv_or_env` — exact
  backup argv/environment, no `RESTIC_PASSWORD`, no password-file contents.
* `init_is_idempotent_and_runs_once` — one `restic init`, second call is
  `already:true`.
* `run_rejects_paths_outside_the_allowlist_and_invalid_tags` — no restic
  invocation on rejected input.
* `snapshots_are_bounded_and_do_not_include_secrets` — limit/truncation/newest
  order and secret-free JSON.
* `restore_rejects_traversal_symlink_and_unknown_roots` — `/etc`, `..`, exact
  root, and symlink escape are typed, with no invocation.
* `restore_uses_exact_argv_and_a_validated_target` — canonical target and exact
  restore argv.
* `delete_is_exact_and_rejects_latest_or_ambiguous_prefixes` — exact-id policy
  and `forget <id> --prune` argv.
* `missing_binary_and_password_permissions_are_typed` — 503/403 typed failures.
* `typed_failures_are_classified` — wrong password maps to
  `backup.password-rejected` without the secret.
* `routes_show_unconfigured_status_and_typed_errors` and
  `routes_return_ready_status_and_real_run_summary` — HTTP shapes.
* `live_restic_init_run_restore_delete_round_trip` — real restic 0.20.0:
  temp repo + temp source (text, 8 KiB binary including every byte value, empty
  file) → init → second init idempotent → run → snapshots → restore to a temp
  restore dir → byte-identical reads → status ready → delete → zero snapshots.
  The test skips cleanly when restic is absent; on this host it passed.

Controller (`python -m pytest -q tests/unit/controller/test_node_api_token_proxy.py`):
80 passed. New cases cover real status JSON, typed `backup.*` errors, snapshot
document preservation, DELETE method/policy, typed missing-binary init, bearer
forwarding, and absence of the token in every response body.

Live HTTP evidence (real node binary, real restic 0.20.0, temp repo/source/
restore dir):

1. `GET /backup/status` → `configured:true`, `repo.state:"uninitialized"`,
   `restic.available:true` at `/usr/bin/restic`.
2. `POST /backup/init` → `initialized:true`, `already:false`.
3. `POST /backup/run {"mode":"smart","tags":["http-live"]}` → snapshot id
   `75c0…5810`, two files, 31 bytes, restic summary included.
4. `GET /backup/snapshots?limit=5` → one snapshot, tags/paths/short_id.
5. `POST /backup/restore {snapshot_id, source_path:"/", target_path:<temp>}` →
   `restored:true`; `diff -r` reported the restored tree byte-identical to the
   source tree.
6. `DELETE /backup/snapshots/<id>` → `deleted`, `pruned:true`,
   `policy:"exact-snapshot-only"`; the next snapshot list was empty.
7. Restore to a path outside the allow-list returned HTTP 403
   `backup.restore-path-denied` with `supported:true` and remediation.

The node log contained only the configured repo, source count, and restore-root
count; no password, password-file path, or secret appeared.

`cargo check -p ozma-node --all-targets` and `cargo test -p ozma-node --lib`
are the required gates; see the task report for final output.

## Remaining gaps

* No app inventory or restore-apps, DB dump hooks, cloud/S3/SFTP/REST
  destinations, Time Machine integration, scheduling/adaptive policy,
  estimate, verify, onboarding, or config mutation.
* No retention policy. Delete is deliberately one-exact-snapshot plus
  unreferenced prune; operators who need daily/weekly/monthly retention should
  use the Python agent or run restic directly.
* No disk-image/ZFS/Btrfs snapshot mode.
* Status/snapshot listing runs restic synchronously for the request; there is
  no background index or cached history.
* Restore preserves the snapshot's absolute path under the target directory;
  it does not restore "in place" to the original host paths.
* Snapshot file browsing (`/backup/snapshots/{id}/files`) remains a typed 501.
* The provider is local-only by design; a future cloud destination needs its
  own credential model and must not reuse this local password path.
