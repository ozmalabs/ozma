# Phase 4 — controller-only node surfaces

Status: RPA and local Restic backup have real, honest subsets; the remaining
controller-only surfaces stay **typed stubs**. No stub returns a fabricated
success, empty buffer, or fake image, and no RPA route invents text or input
results.

This document covers the Rust `ozma-node` side of the controller-only proxies
(`/rpa/*`, `/backup/*`, `/ui/windows`, `/serial/*`, `/rgb/*`, plus `/current`)
and the controller behaviour that preserves typed results.

## Decisions

| Surface | Controller consumer | Decision | Exact provider gap |
|---|---|---|---|
| `/api/v1/rpa/type` | `controller/api.py::_node_rpa_proxy` (`/api/v1/nodes/{id}/rpa/type`) | **Implemented** | Uses the same `SharedSink` and keyboard-layout mapping as `/input/type`. Replies 503 `input-unavailable` when no sink is configured. |
| `/api/v1/rpa/key` | `controller/api.py::_node_rpa_proxy` (`/api/v1/nodes/{id}/rpa/key`) | **Implemented** | Named keys, printable characters, the Python node's integer `modifier` bitmask, `modifiers` names, and the `keys` hotkey form all dispatch HID events through the sink. |
| `/api/v1/rpa/screenshot` (GET/POST) | `controller/api.py::node_rpa_screenshot` / `_node_rpa_proxy` | **Implemented** | Returns the real JPEG from the active capture source; `?console=` uses the same console broker as `/display/snapshot`. Missing sources stay typed errors. |
| `/api/v1/rpa/ocr` (POST) | node-local / future controller callers | **Implemented when tesseract exists** | Captures one real frame and pipes it to the `tesseract` CLI. Without tesseract it returns 503 `rpa.ocr-unavailable`; it never invents text. |
| remaining `/api/v1/rpa/*` (`screen`, `click`, `wait_for_text`, `script`, `enter_bios`, `set_boot_usb`) | `controller/api.py::_node_rpa_proxy` | **Typed stub** (501) | Python `node/rpa.py` has bitmap BIOS OCR, foreground script execution, text waits, click-by-text, and heuristic BIOS navigation. Rust has no script runner or BIOS policy; those routes keep the stable `rpa.unsupported` body. |
| `/backup/{status,init,run,snapshots,restore}` and `DELETE /backup/snapshots/{id}` | `controller/api.py` backup proxy routes | **Implemented (opt-in, local only)** | Opt-in `--backup-repo` + `--backup-password-file` (0600) provider wrapping the local `restic` CLI. Sources and restore targets are explicit allow-lists; no cloud destination, app inventory, DB hooks, scheduling, or retention policy. See [`phase4-backup-report.md`](phase4-backup-report.md). |
| remaining `/backup/*` (`config`, `apps`, `apps/restore`, `estimate`, `verify`, `onboarding`, cloud) | `controller/api.py` backup proxy routes | **Typed stub** (501) | Python `agent/backup.py` owns app inventory, cloud/S3/SFTP/REST destinations, estimate/verify, scheduling, macOS Time Machine, and DB hooks. The Rust node advertises and returns `backup.unsupported` for those sub-surfaces. |
| `/ui/windows` | Python desktop agent (`agent/ozma_desktop_agent.py:996`); the controller consumes `/ui/hints` | **Typed stub** (501, public read-only) | Python `agent/ozma_desktop_agent.py` uses an in-guest accessibility provider. `ozma-node` has no in-guest agent and must not invent window titles or positions. `/ui/hints` keeps its existing empty-shape 200 with `available:false` for compatibility; `/ui/windows` is explicitly unsupported. |
| `/serial/*` | `controller/serial_console.py::_poll_node_serial` and legacy panels | **Typed stub** (501) | Python `node/serial_capture.py` owns a real UART/USB-serial reader, ring buffer, SSE stream, and send path. `ozma-node` has no serialport dependency or serial session implementation. |
| `/rgb/*` | `controller/api.py` RGB proxy, `controller/rgb_outputs.py::_push_node` | **Typed stub** (501) | Python `node/rgb_leds.py` drives WS2812B/SK6812 over SPI and runs effects. `ozma-usb`/`ozma-hid` are gadget/HID injectors, not SPI LED drivers; no WS2812 or SPI crate is used by the node. |
| `/current` (and `/current/history`) | `controller/api.py::node_current` | **Typed stub** (501) | Python `node/current_sensor.py` reads an INA219-class power sensor. No Rust sensor driver or ADC source is present. |
| `GET /storage` | `controller/api.py::node_storage` (`/api/v1/nodes/{id}/storage`) | **Implemented (read-only)** | `/proc/mounts` + `statvfs`, real `/dev` block filesystems only, at most 64 entries. No ZFS/md/multipath/SMART/admin surface. |
| `GET /apps?limit=N` | `controller/api.py::node_apps` (`/api/v1/nodes/{id}/apps`) | **Implemented (read-only)** | Bounded `/proc` pid/name/CPU/RSS/user list, `limit` 1–100 (default 20). Never reads `cmdline` and never kills a process; no parental-control policy. |
| `GET /self` | `controller/api.py::node_self` (`/api/v1/nodes/{id}/self`) | **Implemented (read-only)** | This process's pid, procfs uptime, version/profile, RSS/virtual memory, cgroup unit, and `OZMA_RESTART_COUNT` when exported. No network/Wi-Fi/load/temperature/RTT yet. |

No surface was dropped: the controller routes remain, and every one has either
a real implementation backed by a node primitive or a discoverable typed result.

### Why the remaining surfaces are not implemented

- RPA **type/key/screenshot** reuse existing, tested primitives: the shared
  input sink (`/input/*`), keyboard-layout mapper, capture registry, console
  broker, and JPEG encoder.
- RPA **OCR** is a thin, bounded wrapper around the system `tesseract` CLI. It
  adds no Rust OCR dependency and never fabricates output; absent tesseract is a
  typed 503.
- RPA **script/BIOS/wait/click** require a policy engine and, for bitmap BIOS
  OCR, an embedded font matcher. That is not present in the Rust node, so those
  routes remain typed 501s rather than pretending a script or BIOS action ran.
- `ozma-usb` and `ozma-hid` expose ConfigFS gadget, mass-storage/NBD, and HID
  injection primitives. They do not contain an SPI/WS2812 LED output path.
- The node has no `serialport`/`tokio-serial` dependency and no serial session
  buffer to expose.
- Backup **status/init/run/snapshots/restore/delete** are implemented in
  `ozma-node/src/backup.rs` as a bounded, opt-in wrapper around the local
  `restic` CLI. Only explicitly allow-listed sources and restore roots are
  accepted; every platform-global default, cloud destination, app inventory,
  DB hook, scheduler, and retention policy remains absent and typed 501.
- `/ui/windows` requires an in-guest accessibility agent, not a node library.

Implementing a partial surface (for example RGB state tracking without hardware
output, or a script runner that logs but does not execute) would create exactly
the fake success this work is meant to remove.

## Implemented RPA behaviour

### `POST /api/v1/rpa/type`

Accepts `{"text": "...", "delay": 0.03}`. Text is validated against the
configured guest layout (`--keyboard-layout us|uk|de|fr`) before any event is
sent, then typed through the shared sink exactly like `/input/type`. `delay` is
optional, bounded to 100 ms per character, and defaults to the existing
`/input/type` timing. Unsupported characters, dead keys without a base, or
oversized text return 400 and type nothing. No sink returns 503
`input-unavailable`.

### `POST /api/v1/rpa/key`

Accepts the Python node shape `{"key": "enter", "modifier": 2}` plus controller
combo forms:

```json
{"key": "enter"}
{"key": "?", "modifier": 2}
{"key": "c", "modifiers": ["ctrl"]}
{"keys": ["ctrl", "c"]}
```

Named keys mirror `node/rpa.py` (`enter`, `esc`, `f1`..`f12`, arrows, paging,
delete, etc.). A single printable character is resolved through the selected
keyboard layout, including its Shift/AltGr requirements. Unknown names,
ambiguous combos, or unsupported characters return 400 and send nothing.

### `GET|POST /api/v1/rpa/screenshot`

Returns `image/jpeg` bytes from the configured capture source, with
`Cache-Control: no-store`. `?console=Console_N` (or an index) uses the same
console broker and active-console rules as `/display/snapshot`. No source,
unknown console, single-head conflict, capture timeout, or encode failure is a
typed error (`capture.unavailable`, 404, 409, 504, or 503 as appropriate), never
a placeholder image.

### `POST /api/v1/rpa/ocr`

Captures one frame and runs `tesseract stdin stdout --psm 3 -l eng` with the
JPEG piped on stdin. Bounds:

- 16 MiB maximum JPEG
- 8192x4096 maximum pixel count
- 20 s invocation timeout
- 1 MiB maximum stdout

The response contains the exact engine text (`text` and `full_text`), the
capture geometry/source, and an empty `text_regions` array because this route
does not compute coordinates. An empty screen returns an empty string. Missing
tesseract returns 503 `rpa.ocr-unavailable`; oversized frames return 413
`rpa.ocr-frame-too-large`; timeouts return 504 `rpa.ocr-timeout`; process
failures return 502 `rpa.ocr-failed`.

## Implemented local backup behaviour

The provider is opt-in and local-only. Without `--backup-repo`, the core routes
are still mounted: `GET /backup/status` reports `configured:false` and every
mutating operation returns 503 `backup.not-configured`. Unsupported sub-surfaces
remain typed 501s.

Flags:

| Flag | Meaning |
|---|---|
| `--backup-repo PATH` | Absolute local restic repository. Remote backends (`s3:`, `sftp:`, `rest:`, …) are rejected. |
| `--backup-password-file PATH` | Absolute regular file, mode `0600`. Passed only as `RESTIC_PASSWORD_FILE`; never in argv, logs, or status. |
| `--backup-source PATH` | Repeatable complete source allow-list. No defaults; exact `/`, `/etc`, `/home`, `/var`, `/tmp`, etc. are rejected. |
| `--backup-restore-dir PATH` | Repeatable complete restore-target allow-list. Required for restore. |
| `--backup-restic PATH` | Optional restic executable override (default `restic`). |

Endpoints (all guarded):

| Endpoint | Behaviour |
|---|---|
| `GET /backup/status` | `configured`/`repo.state` (`uninitialized` or `ready`), restic availability, source/restore-root lists, last snapshot, last run/failure code. |
| `POST /backup/init` | Idempotent `restic init`; an existing repository is reported with `already:true` and no second invocation. |
| `POST /backup/run` | Bounded `{"mode","paths","tags"}` request. Paths must resolve under a configured source; tags are `[A-Za-z0-9._-]{1,64}` (max 8); paths max 16. Returns restic's JSON summary plus snapshot id. `smart` means configured sources only — no app/DB/cloud/disk-image work. |
| `GET /backup/snapshots?limit=N` | Bounded snapshot list (default 20, max 100), newest first, with only id/short_id/time/hostname/tags/paths/program_version. |
| `POST /backup/restore` | Explicit snapshot (or `latest`) and target below a restore root. Restores through `restic restore`; `..`, exact-root targets, and symlink components are rejected. |
| `DELETE /backup/snapshots/{id}` | Requires an 8–64 hex snapshot id (never `latest`), verifies it exists, then runs `restic forget <id> --prune`. Only unreferenced data is pruned; no retention rule touches other snapshots. |

Exact argv (no shell): `restic --no-cache init`; `restic --no-cache backup
--json [--tag T]... PATH...`; `restic --no-cache snapshots --json`; `restic
--no-cache restore ID --target PATH [--include PATH]`; `restic --no-cache
forget ID --prune`. Each child gets a cleared environment with only
`RESTIC_REPOSITORY`, `RESTIC_PASSWORD_FILE`, `RESTIC_PROGRESS_FPS`, a minimal
`PATH`, and `LANG`/`LC_ALL=C`.

Typed errors (`ok:false`, `supported:true`, stable `code`, `error`,
`remediation`): `backup.restic-missing` (503), `backup.not-configured` (503),
`backup.repo-invalid`/`backup.password-file` (400), `backup.password-permissions`
(403), `backup.password-rejected` (502), `backup.repo-uninitialized` (409),
`backup.permission`/`backup.restore-path-denied` (403),
`backup.restore-not-configured` (503), `backup.path-not-allowed`/
`backup.snapshot-invalid`/`backup.tag-invalid`/`backup.mode-unsupported` (400 or
501), `backup.busy` (409), `backup.restic-failed` (502), and `backup.timeout`
(504).

## Host observability surfaces

`ozma-node` also serves three bounded, guarded read-only routes for the agent
storage/app-monitor/self-management gap:

| Node route | Returns | Privacy/limit |
|---|---|---|
| `GET /storage` (`/api/v1/storage`) | `device`, `mount`, `fs`, total/used/free bytes, percent | `/dev` block mounts only, at most 64, no secrets, `statvfs` read-only |
| `GET /apps?limit=N` (`/api/v1/apps`) | `pid`, `name`, `cpu_percent` (+ window), `rss_bytes`, `user`, `uid` | scan ≤16,384 proc entries, return ≤100 (`limit` 1–100, default 20); **never** `cmdline`, env, or kill |
| `GET /self` (`/api/v1/self`) | pid, process/host uptime, version, profile, virtual/RSS memory, unit, restart count | no invented restart count: only `OZMA_RESTART_COUNT`; missing facts are `null`/`unavailable` |

All three are mounted in `compat_router` (behind the node bearer guard) next to
`/hardware`, and an unreadable procfs/source is a typed `503`
(`storage.proc-unavailable`, `apps.proc-unavailable`,
`self.proc-unavailable`). Control characters in mount/process labels are
replaced, mount-table octal escapes are decoded, and per-process CPU uses a
documented `since-boot` or `since-last-sample` window.

`GET /api/v1/connection` (and `/node/status`) advertises them in
`host_surfaces` with `supported`, `path`, `available`, `guarded`, `reason`, and
`limit`. Availability is captured at startup with cheap procfs opens; building
the connection document runs no mount/process scan and exposes no pid, mount,
username, or unit.

The controller adds minimal `_proxy_node_surface` pass-throughs at
`/api/v1/nodes/{node_id}/{storage,apps,self}`; `apps` forwards `limit` and the
node clamps it. Typed `storage.*`/`apps.*`/`self.*` node errors keep their 503
status and JSON body, and the node bearer token is forwarded. The Python agent did not expose `/storage` or `/apps` over HTTP;
`node/self_management.py` served its report at `/node/status`, which the Rust
node keeps as the connection summary. That consumer mismatch
(`mesh_intelligence` reads Python self-report keys from `/node/status`) is
documented as a remaining gap in
[`docs/phase4-storage-apps-report.md`](phase4-storage-apps-report.md).

Rust tests live in `host_surfaces::tests` (parsing, filters, limits, empty
sources, CPU windows, privacy, HTTP JSON shape, typed errors, live host
collectors). Controller tests add the three proxy routes and `limit`
forwarding to `tests/unit/controller/test_node_api_token_proxy.py`.

## Typed stub behaviour

Every unsupported surface answers `HTTP 501 Not Implemented` with
`Content-Type: application/json` and a stable body:

```json
{
  "ok": false,
  "code": "rpa.unsupported",
  "surface": "/api/v1/rpa/*",
  "capability": "rpa",
  "supported": false,
  "reason": "This node has no RPA script runner, wait_for_text, bitmap screen OCR, click helper, or BIOS automation.",
  "error": "This node has no RPA script runner, wait_for_text, bitmap screen OCR, click helper, or BIOS automation.",
  "python_reference": "node/rpa.py (node/node.py:437-445)",
  "remediation": "Use the implemented /api/v1/rpa/{type,key,screenshot,ocr} routes, ..."
}
```

`code`, `surface`, `capability`, `supported`, `reason`, and
`python_reference` are the stable fields. `error` mirrors `reason` for existing
controller error handling; `remediation` is operator guidance.

Stable codes:

| Surface | Code |
|---|---|
| remaining RPA sub-surfaces | `rpa.unsupported` |
| OCR absent | `rpa.ocr-unavailable` |
| remaining backup sub-surfaces | `backup.unsupported` |
| `ui_windows` | `ui.windows_unsupported` |
| `serial` | `serial.unsupported` |
| `rgb` | `rgb.unsupported` |
| `current` | `power.current_unsupported` |

Routes:

- guarded control routers (`controller_compat::rpa_router`):
  `/api/v1/rpa/type`, `/api/v1/rpa/key`, `/api/v1/rpa/screenshot`,
  `/api/v1/rpa/ocr`
- guarded implemented backup router (`backup::router`) and typed 501 fallback
  (`controller_compat::legacy_router`): `/backup/{status,init,run,snapshots,restore}`
  and `DELETE /backup/snapshots/{id}` are implemented; remaining `/backup/*`
  (`config`, `apps`, `apps/restore`, `estimate`, `verify`, `onboarding`, cloud)
  keep the stable `backup.unsupported` body.
- guarded typed 501 fallback (`controller_compat::legacy_router`): remaining
  `/api/v1/rpa/*`, `/serial/*`, `/rgb/state`, `/rgb/effect`, `/rgb/set`,
  `/rgb/off`, `/current`, `/current/history`
- public read-only router (`controller_compat::public_router`): `/ui/windows`

Auth is unchanged from the existing node routes: control surfaces, including all
RPA routes, stay behind the node bearer guard; `/ui/windows` is public
read-only, matching `/ui/hints`. The 501 stubs never inspect or mutate hardware
state.

## Discovery without probing

`GET /api/v1/connection` carries `controller_surfaces`, generated from
`controller_compat::RPA_IMPLEMENTED_SURFACES` (now also carrying the local
backup core endpoints) plus `controller_compat::CONTROLLER_ONLY_SURFACES`:

```json
"controller_surfaces": [
  {
    "surface": "/api/v1/rpa/ocr",
    "capability": "rpa.ocr",
    "supported": true,
    "reason": "Runs the optional tesseract CLI on a captured frame; without tesseract the route returns 503 rpa.ocr-unavailable.",
    "python_reference": "node/rpa.py::NodeRPA.read_screen (node/node.py:382-393)"
  },
  {
    "surface": "/api/v1/rpa/*",
    "capability": "rpa",
    "supported": false,
    "reason": "This node has no RPA script runner, wait_for_text, bitmap screen OCR, click helper, or BIOS automation.",
    "python_reference": "node/rpa.py (node/node.py:437-445)"
  }
]
```

A controller can read this document once and distinguish implemented primitives
(`supported:true`) from typed absences (`supported:false`) without probing. The
array is additive; existing connection fields are unchanged.

## Controller behaviour

`controller/api.py` keeps every route and preserves the node's typed results:

- `_proxy_node_surface` passes a typed 501 body through as JSON. It is used by
  `/current`, `/rgb/state`, `/rgb/set`, `/ui/hints`, and the RPA proxy.
- `_proxy_backup_surface` forwards real backup JSON verbatim and preserves
  `backup.*` typed errors (missing restic, bad repo/password, allow-list/restore
  escapes, restic failures) with their node status. The node bearer token is
  forwarded only in the request header; it is never copied into the response or
  error detail.
- `_proxy_node_raw` fetches `/api/v1/rpa/screenshot` as bytes and checks the
  status first. A typed error JSON body is returned as JSON, never labelled
  `image/jpeg`; a real 200 JPEG streams through unchanged.
- Node 401/403 still maps to the existing token-free controller 502
  explanation; non-typed node failures remain 502, not a hang or a fake 200.
- The backup proxies do not turn a typed 501 into the generic 503
  "agent unreachable" message, and `/api/v1/backup/nodes/{id}/{status,init}`
  plus `DELETE .../snapshots/{id}` use the same token-free typed-error path.
- `/ui/hints` does not return `200 {"error": ...}` when the agent is absent; it
  forwards the typed body (or a real 502 on transport failure).

No controller proxy route was removed; the existing typed-error and JPEG
pass-through paths still handle both outcomes, and the local backup core uses a
dedicated typed-error-preserving helper.

## Tests

Rust (`cargo test -p ozma-node --lib`):

- `controller_compat::tests::rpa_type_route_dispatches_layout_aware_events_through_the_sink`
  asserts the exact HID event sequence, sink/injected fields, and the typed
  503 with no sink.
- `controller_compat::tests::rpa_key_route_supports_named_keys_characters_and_combos`
  covers named keys, `keys` combos, layout-resolved characters, the integer
  modifier bitmask, invalid keys, and the no-sink 503.
- `controller_compat::tests::rpa_screenshot_returns_real_jpeg_and_honours_console_selection`
  proves default and `?console=` JPEG bytes decode at the expected geometry and
  that a missing source stays a typed error.
- `controller_compat::tests::rpa_ocr_returns_exactly_the_engine_text` uses a fake
  CLI to prove the route returns exactly the engine's text and frame geometry.
- `controller_compat::tests::rpa_ocr_runs_installed_tesseract_over_a_real_frame`
  runs the real tesseract path and skips when the binary is absent.
- `controller_compat::tests::rpa_ocr_without_tesseract_is_a_typed_503` covers
  `rpa.ocr-unavailable`.
- `controller_compat::tests::controller_only_surfaces_are_advertised_and_return_typed_501s`
  distinguishes implemented RPA surfaces from unsupported surfaces and checks
  the remaining typed 501s.
- `controller_compat::tests::legacy_routes_are_typed_501s_with_remediation`
  covers the remaining RPA script/BIOS aliases plus RGB/current/serial and the
  unsupported backup config/app sub-surfaces.
- `controller_compat::tests::backup_core_is_typed_not_configured_without_a_repo`
  proves the core backup routes stay mounted with `configured:false` and typed
  503s instead of fabricated snapshots.
- `backup::tests::*` cover exact restic argv/environment, secret absence, init
  idempotence, path/tag/restore allow-lists, traversal/symlink rejection,
  bounded snapshots, exact-snapshot delete policy, typed missing
  binary/password/permission failures, and a live restic init → run → snapshots
  → restore (byte-identical) → delete round trip.
- `controller_compat::tests::input_codec_and_audio_routes_are_merged_behind_the_bearer_guard`
  covers the bearer guard for every implemented RPA route and the remaining
  typed stubs.
- `rpa::tests::*` cover named-key mapping, modifier ordering, exact event
  construction, unavailable OCR, and oversized-frame rejection.

Controller (`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- `test_controller_only_surface_proxies_preserve_typed_unsupported` still covers
  a node that returns a typed 501 for RPA screen/screenshot/key, current, RGB,
  backup, and UI hints, including bearer-token propagation.
- `test_backup_status_proxy_preserves_real_json_without_token_leak`,
  `test_backup_proxy_preserves_typed_errors_without_token_leak`,
  `test_backup_snapshots_proxy_preserves_node_document`,
  `test_backup_delete_proxy_uses_delete_and_preserves_policy`, and
  `test_backup_init_proxy_preserves_typed_missing_binary` prove real JSON and
  `backup.*` typed errors survive without copying the bearer token into a
  response.
- `test_rpa_screenshot_still_returns_real_jpeg_bytes` proves a real node
  screenshot is not broken by the typed-error path.

## Remaining gaps

- RPA script execution, `wait_for_text`, click-by-text, bitmap BIOS OCR, and
  BIOS navigation (`enter_bios`, `set_boot_usb`) remain Python-node-only. The
  Rust node keeps the typed 501 body for those routes.
- OCR requires the system `tesseract` binary and the `eng` language data. A node
  without it reports `rpa.ocr-unavailable` instead of returning empty/guessed
  text.
- The Python providers remain the only implementations of app inventory,
  cloud/S3/SFTP/REST destinations, backup scheduling/verification/estimate,
  serial capture, RGB output, current sensing, and in-guest window hints. The
  Rust local provider deliberately stops at local init/run/snapshots/restore/
  exact-snapshot delete.
- The node registration capability list (`capabilities` TXT/registration field)
  is owned by `main.rs` and is not changed here; discovery uses the connection
  document. A later registration change can mirror the same capability names.
- `controller/serial_console.py` only polls nodes that advertise `cap:serial`
  and `controller/rgb_outputs.py` only pushes to explicitly configured node
  zones, so neither currently probes a Rust node that lacks these capabilities.
  Their existing failure paths log and do not fabricate success.
