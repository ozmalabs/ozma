# Phase 4 — local Restic backup report

Status: **implemented (2026-11-26).** `ozma-node` now has an opt-in local Restic
provider at `/backup/status`, `/backup/init`, `/backup/run`,
`/backup/snapshots`, `/backup/restore`, `DELETE /backup/snapshots/{id}`,
`GET /backup/stats`, `POST /backup/verify`, and `POST /backup/estimate`. It
replaces only the core of the previous typed `/backup/*` 501 stub; app
inventory/restore-apps, cloud destinations, config writes, onboarding,
scheduling, DB hooks, and retention policy remain typed 501s.

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
  100 snapshots per list. Verify accepts only a 1–5% read-data subset (default
  1%); stats, verify, and estimate each have their own timeout; stdout/stderr
  are read with hard caps; `kill_on_drop` stops a timed-out child.
* **Serialized heavy operations.** A concurrent init/run/restore/delete/verify/
  estimate returns 409 `backup.busy`; read-only status/snapshot/stats polls do
  not hold the operation lock. Verify and estimate are serialized with
  mutations because both run restic commands that acquire the repository lock.

### Exact restic argv

```
restic --no-cache init
restic --no-cache backup --json [--tag TAG]... PATH...
restic --no-cache snapshots --json
restic --no-cache restore SNAPSHOT --target TARGET [--include SOURCE_PATH]
restic --no-cache forget SNAPSHOT_ID --prune
restic --no-cache stats --mode MODE --json
restic --no-cache check --read-data-subset PERCENT% --json
restic --no-cache backup --dry-run --json PATH...
```

`--include` is omitted when the snapshot source path is `/` (whole-snapshot
restore). Restore preserves absolute snapshot paths below `--target`, matching
restic semantics. Estimate reuses the same source allow-list and path
resolution as a real run but adds `--dry-run`, so it never writes a snapshot.
Verify builds the subset from a validated integer (never a raw request string),
so only `1%`–`5%` reach argv.

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

### `GET /backup/stats`

Runs three `restic stats --json` modes and copies only known numeric fields:

* `snapshots.count` — snapshots currently in the repository.
* `unique.size_bytes` / `unique.file_count` — unique-content files
  (`--mode files-by-contents`).
* `restore.size_bytes` / `restore.file_count` — logical restore size
  (`--mode restore-size`).
* `repository.size_bytes` / `uncompressed_size_bytes` / `blob_count` — packed
  repository data (`--mode raw-data`), plus compression ratio/space-saving when
  the repository reports them.
* `measured_with:"restic-stats"` and `generated_at`.

Unknown restic fields are dropped rather than passed through. The response
contains no password, environment, or raw restic stderr. Stats is read-only and
does not hold the operation lock, so it can run alongside status/snapshot
polls.

### `POST /backup/verify`

Runs `restic check --read-data-subset N% --json` with `N` in `1..=5` (default
1). The body is optional:

```json
{"read_data_percent": 2}
```

`read_data_subset` also accepts `"2%"`/`"5%"`; anything else, including
`n/t` and byte-size forms, is rejected with 400 `backup.subset-invalid` before
restic runs. The raw request value is never placed in argv. Verify is
serialized with mutations and can return 409 `backup.busy`.

A successful response is only produced when restic exits 0 **and** its JSON
summary reports `num_errors:0`:

```json
{
  "ok": true,
  "supported": true,
  "verified": true,
  "integrity": "restic-check",
  "read_data_subset": "2%",
  "read_data_percent": 2,
  "errors": 0,
  "summary": {"num_errors":0,"broken_packs":[],"suggest_repair_index":false,"suggest_prune":false}
}
```

A non-zero exit, a damaged-pack summary, or a missing summary is a typed
failure (`backup.restic-failed`, or `backup.repo-uninitialized` /
`backup.password-rejected` for restic exit 10/12). There is no "started" or
optimistic success shape.

### `POST /backup/estimate`

Runs `restic backup --dry-run --json` over the configured sources or an
optional bounded `paths` list. Paths use the same canonicalization and
`--backup-source` allow-list as `POST /backup/run`; outside paths return 400
`backup.path-not-allowed` without invoking restic.

```json
{"paths": ["/srv/data/docs"]}
```

The response reports restic's scanned total, not a fabricated number:

```json
{
  "ok": true,
  "supported": true,
  "estimated": true,
  "method": "restic-backup-dry-run",
  "paths": ["/srv/data/docs"],
  "estimated_bytes": 12345,
  "file_count": 5,
  "summary": {"total_files_processed":5,"total_bytes_processed":12345}
}
```

The dry-run snapshot id is removed. An uninitialized repository returns 409
`backup.repo-uninitialized`; no sources returns 409 `backup.no-sources`; a
restic version without `backup --dry-run` returns 501
`backup.estimate-unsupported`; a dry run without a summary is a typed
`backup.restic-failed` rather than a zero estimate.

### `GET /backup/apps` / `POST /backup/apps/restore`

These stay HTTP 501 with the stable `backup.unsupported` document. A restic
repository records file trees, not installed OS packages or app definitions, so
this provider cannot derive an honest app inventory. The response includes an
app-inventory-specific `reason` and remediation and never includes an `apps` or
`restored` success field.

### Other unsupported sub-surfaces

`/backup/config`, `/backup/onboarding`, snapshot file browsing, and any cloud
destination remain HTTP 501 with the stable `backup.unsupported` document.
`GET /api/v1/connection.controller_surfaces` advertises the nine implemented
core capabilities (status/init/run/snapshots/restore/delete/stats/verify/
estimate) as `supported:true` and the remaining `/backup/*` surface as
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
| `backup.subset-invalid` | 400 | Verify subset outside 1–5% or non-integer subset syntax. |
| `backup.estimate-unsupported` | 501 | restic lacks `backup --dry-run --json`. |
| `backup.busy` | 409 | Another mutating/verify/estimate operation is running. |
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
`DELETE .../snapshots/{snapshot_id}`, `POST .../restore`, `POST .../run`,
`GET .../stats`, `POST .../verify`, and `POST .../estimate`.
`POST .../run` forwards bounded `paths`/`tags` and defaults `mode` to `smart`.
`POST .../verify` and `POST .../estimate` forward an optional JSON object
verbatim (no client-side defaults that would hide a typed node error), and
`GET .../stats` returns the node document unchanged. The existing apps and
restore-apps proxies preserve the node's typed 501. `_proxy_node_response`
sends the actual HTTP method (`DELETE` included) instead of always issuing a
GET for non-POST methods.

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
* `stats_extracts_known_numbers_and_never_passes_extra_fields_through` — exact
  three `stats --mode` argv forms, bounded numeric fields, and no passthrough of
  an unexpected/secret field.
* `verify_uses_a_bounded_read_data_subset_and_reports_success_only_from_restic`
  — exact `check --read-data-subset` argv, default/2%/5% construction.
* `verify_failures_never_claim_integrity` — invalid subsets invoke nothing;
  damaged-pack summaries and non-zero exits return typed failures without
  `verified`.
* `estimate_uses_the_run_allow_list_and_dry_run_argv` — exact dry-run argv,
  scanned byte/file counts, dry-run snapshot id removed, outside paths rejected
  before restic.
* `estimate_reports_unsupported_restic_versions_and_missing_summaries_typed` —
  `backup.estimate-unsupported` and no fabricated zero estimate.
* `app_inventory_surfaces_stay_typed_501_and_do_not_invent_apps` — apps and
  restore-apps expose no `apps`/`restored` success field.
* `stats_verify_estimate_routes_are_typed_not_configured_without_a_repo` —
  503 `backup.not-configured` for all three new routes.
* `routes_expose_stats_verify_and_estimate_documents` — configured HTTP shapes
  for all three new routes, including request-body parsing.
* `routes_show_unconfigured_status_and_typed_errors` and
  `routes_return_ready_status_and_real_run_summary` — HTTP shapes.
* `live_restic_init_run_restore_delete_round_trip` — real restic 0.20.0:
  temp repo + temp source (text, 8 KiB binary including every byte value, empty
  file) → init → second init idempotent → run → snapshots → restore to a temp
  restore dir → byte-identical reads → status ready → delete → zero snapshots.
  The test skips cleanly when restic is absent; on this host it passed.
* `live_restic_stats_estimate_verify_and_damaged_pack_failure` — real restic
  0.20.0: init → run → estimate ≥ scanned bytes → stats snapshot/repository/
  unique/restore counts → verify 1% with zero errors → remove one real pack
  file → verify returns `backup.restic-failed` and never reports `verified`.
  On this host it passed; it skips cleanly when restic is absent.

Full `cargo test -p ozma-node --lib`: **655 passed, 4 ignored** (the ignored
tests require optional GStreamer/media helpers). The workspace copy used for
this run was a clean HEAD worktree because another in-progress edit outside
this task's scope had temporarily broken `ozma-multiseat`; no file from that
crate was modified by this task.

Controller (`python -m pytest -q tests/unit/controller/test_node_api_token_proxy.py`):
90 passed. Cases cover real status JSON, typed `backup.*` errors, snapshot
document preservation, DELETE method/policy, typed missing-binary init, stats
document preservation, verify body/typed-subset forwarding, estimate
body/typed-unsupported forwarding, bearer forwarding, and absence of the token
in every response body.

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

New-route evidence for this increment is the live provider test
`live_restic_stats_estimate_verify_and_damaged_pack_failure` (real restic 0.20.0
temp repo) plus the controller proxy pytest cases. It covers real stats numbers,
estimate scanned bytes, verify success at 1%, and a real removed-pack verify
failure that returns `backup.restic-failed` rather than success. The node log
contained only the configured repo, source count, and restore-root count; no
password, password-file path, or secret appeared.

`cargo check -p ozma-node --all-targets` and `cargo test -p ozma-node --lib`
are the required gates; see the task report for final output.

## Remaining gaps

* No app inventory or restore-apps (a restic repository cannot supply an OS
  package/app inventory), DB dump hooks, cloud/S3/SFTP/REST destinations, Time
  Machine integration, scheduling/adaptive policy, onboarding, or config
  mutation.
* No retention policy. Delete is deliberately one-exact-snapshot plus
  unreferenced prune; operators who need daily/weekly/monthly retention should
  use the Python agent or run restic directly.
* No disk-image/ZFS/Btrfs snapshot mode.
* Status/snapshot listing/stats/verify/estimate run restic synchronously for the
  request; there is no background index, cached history, verify job queue, or
  progress streaming.
* Verify reads only 1–5% of pack data per request. Repeated calls are needed for
  broader coverage; this provider never claims a full-repository data scrub.
* Estimate is a dry-run scan, not a predictive model: it reports the scanned
  byte count and still costs I/O proportional to the selected sources.
* Restore preserves the snapshot's absolute path under the target directory;
  it does not restore "in place" to the original host paths.
* Snapshot file browsing (`/backup/snapshots/{id}/files`) remains a typed 501.
* The provider is local-only by design; a future cloud destination needs its
  own credential model and must not reuse this local password path.
