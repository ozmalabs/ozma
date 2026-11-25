# Phase 4 — read-only storage health (SMART and mdraid)

Status: **read-only observability implemented; no SMART tests, md
administration, writes, or mounts.**

`ozma-node` now exposes two guarded, bounded host surfaces that answer the
operator question "are the disks healthy?" without becoming a storage
administration API:

| Route | Alias | Source | Bound |
|---|---|---|---|
| `GET /storage/health` | `GET /api/v1/storage/health` | `/sys/block`, `smartctl -j -H -A`, `/proc/mdstat`, optional `mdadm --detail --scan` | 32 devices, 32 arrays, 64 members/array, 64 attributes |
| `GET /storage/smart?device=/dev/nvme0n1` | `GET /api/v1/storage/smart` | `/sys/block`, one bounded `smartctl -j -H -A` call | one device, 1 MiB smartctl stdout |

Both routes are mounted in `controller_compat::compat_router` next to
`/storage`, so they use the node bearer-token guard. They are not public. The
`/api/v1/connection.host_surfaces` advertisement gains `health` and `smart`
capabilities captured with cheap startup checks (`/sys/block` directory and a
`PATH`/absolute executable lookup); building the connection document still
does not run `smartctl` or `mdadm`.

## What is read and what is never done

`/storage/health` reads:

- `/sys/block/<name>/{size,queue/rotational,device/model,device/serial}`;
- `smartctl -j -H -A <trusted /dev path>` once per physical block device;
- `/proc/mdstat`;
- `mdadm --detail --scan` when `mdadm` is available.

`/storage/smart` reads the same sysfs facts for one device and one `smartctl`
call.

The implementation never runs SMART self-tests (`-t short`, `-t long`), never
runs `mdadm --create/--assemble/--start/--stop/--add/--remove`, never writes a
device, never mounts or unmounts, never formats, and never scans ZFS. Command
output is capped and has a deadline (5 s for `smartctl`, 3 s for `mdadm`).
`smartctl` is always invoked on a path rebuilt from a validated basename under
`/dev`; the raw query string is never passed through as a path.

## `GET /storage/health`

The document reports the bounded inventory, a top-level SMART source status, an
mdraid section, and the optional sections that were unavailable:

```json
{
  "ok": true,
  "available": true,
  "source": "/sys/block + smartctl + /proc/mdstat",
  "count": 1,
  "matched": 1,
  "limit": 32,
  "truncated": false,
  "serials_included": false,
  "attributes_included": false,
  "smart": {"available": true},
  "md": {
    "available": true,
    "source": "/proc/mdstat",
    "mdadm_available": true,
    "count": 0,
    "arrays": []
  },
  "unavailable": [],
  "devices": [
    {
      "name": "nvme0n1",
      "device": "/dev/nvme0n1",
      "model": "Fanxiang S880E 1TB",
      "size_bytes": 1000204886016,
      "rotational": false,
      "smart": {
        "available": true,
        "passed": true,
        "temperature_c": 49,
        "percentage_used": 21,
        "available_spare_percent": 100,
        "media_errors": 0,
        "critical_warning": 0,
        "power_on_hours": 1443,
        "protocol": "NVMe"
      }
    }
  ]
}
```

### Block-device inventory

Only direct children of `/sys/block` with safe basenames are considered.
Kernel RAM/loop pseudo-disks (`ram*`, `zram*`, `loop*`) are excluded because
they are not storage-health candidates. The list is sorted and capped at 32
entries; `matched` reports how many candidates existed before the bound and
`truncated` says whether the cap removed any. The output is bounded even when
sysfs contains thousands of loop devices.

For each device:

- `name` is the sysfs basename; `device` is `/dev/<name>`.
- `model` is sysfs `device/model`, sanitized and capped at 64 characters.
- `size_bytes` is `size * 512` (sysfs reports 512-byte sectors).
- `rotational` is `queue/rotational != 0` when present.
- `serial` is **omitted by default**; see the privacy policy below.
- `smart` is the normalized SMART document described next.

### SMART normalization

`smartctl` JSON is parsed into a small fixed schema. Raw JSON is never
returned. The normalized fields are:

| Field | Source | Meaning |
|---|---|---|
| `available` | smartctl data presence | usable SMART health was returned |
| `passed` | `smart_status.passed` | overall SMART verdict |
| `temperature_c` | `temperature.current` or NVMe log | current temperature |
| `percentage_used` | NVMe log or `endurance_used` | NVMe endurance consumed |
| `available_spare_percent` | NVMe log or `spare_available` | NVMe spare capacity |
| `media_errors` | NVMe log | NVMe integrity errors |
| `critical_warning` | NVMe log | NVMe critical-warning bits |
| `reallocated_sectors` | ATA attribute 5 | reallocated sector count |
| `pending_sectors` | ATA attribute 197 | current pending sectors |
| `uncorrectable_sectors` | ATA attribute 198 | offline uncorrectable sectors |
| `power_on_hours` | NVMe log or `power_on_time.hours` | device age |
| `model`, `protocol` | smartctl device metadata | bounded labels |
| `attributes` | `ata_smart_attributes.table` | only with `?attributes=true`, at most 64 normalized rows |

An unavailable per-device SMART section is still a `200` on
`/storage/health`; the device carries:

```json
{"available": false, "code": "storage.smart-unavailable", "error": "smartctl was not found on PATH; install smartmontools to read SMART health"}
```

The top-level `smart.available` is true only when at least one device yielded
usable SMART data. A present tool that cannot read any device (for example an
unprivileged node) is still reported as typed `storage.smart-unavailable`
while the inventory and md sections remain available.

Examples of honest unavailability are a missing `smartctl`, a
non-root caller that cannot open the device, an unsupported USB bridge, or a
malformed/truncated smartctl document. `/storage/health` keeps the inventory
and md data in those cases.

### mdraid visibility

`/proc/mdstat` is parsed into bounded arrays:

```json
{
  "name": "md0",
  "device": "/dev/md0",
  "state": "active",
  "level": "raid1",
  "degraded": false,
  "total_devices": 2,
  "active_devices": 2,
  "size_bytes": 2000139190272,
  "slot_map": "UU",
  "uuid": "1234:5678:9abc:def0",
  "metadata": "1.2",
  "members": [
    {"device": "sda1", "slot": 0, "state": "active"},
    {"device": "sdb1", "slot": 1, "state": "active"}
  ],
  "sync": {"operation": "resync", "percent": 42.5, "speed_kib": 150000, "finish_minutes": 12.3}
}
```

- `degraded` is true when `active_devices < total_devices`, when the slot map
  contains `_`, or when a member is marked `(F)` failed.
- `slot_map` is the bounded `[UU]`/`[U_]` kernel summary.
- `sync` appears only for an in-progress resync/recovery/reshape/check.
- `uuid` and `metadata` are attached only when a bounded
  `mdadm --detail --scan` call succeeds; `/proc/mdstat` remains the source of
  truth for live state and members.
- Arrays are capped at 32; members at 64 per array.

If `/proc/mdstat` does not exist or cannot be read, the md section is a typed
`storage.md-unavailable` and the rest of `/storage/health` stays a `200`:

```json
{
  "available": false,
  "code": "storage.md-unavailable",
  "error": "could not read /proc/mdstat: No such file or directory",
  "source": "/proc/mdstat",
  "mdadm_available": false,
  "count": 0,
  "arrays": []
}
```

This is common on hosts that boot without the md module and is the live
behaviour on the reference host. If `mdadm` is missing but `/proc/mdstat` is
readable, the arrays are still reported with `mdadm_available: false` and a
bounded `mdadm_error`.

## `GET /storage/smart?device=/dev/nvme0n1`

The per-device route exists for an explicit operator/dashboard drill-down. It
returns one normalized document and no raw smartctl output:

```json
{
  "ok": true,
  "available": true,
  "source": "/sys/block + smartctl",
  "device": "/dev/nvme0n1",
  "name": "nvme0n1",
  "model": "Fanxiang S880E 1TB",
  "serials_included": false,
  "attributes_included": false,
  "size_bytes": 1000204886016,
  "rotational": false,
  "protocol": "NVMe",
  "smart": {
    "available": true,
    "passed": true,
    "temperature_c": 49,
    "percentage_used": 21,
    "available_spare_percent": 100,
    "media_errors": 0
  }
}
```

### Device validation

`device` is mandatory and must be exactly one block-device basename under
`/dev`:

- reject anything that does not start with `/dev/`;
- reject empty names, names longer than 64 characters, names with a slash,
  backslash, control character, leading dot, or `..`;
- reject non-`Normal` path components;
- require the basename to exist under `/sys/block` or `/dev`;
- rebuild the smartctl argument as `<dev-root>/<validated-name>`.

There is no query-string path traversal, no shell, and no argument injection.
Examples that return a typed `400 storage.invalid-device`:

- `device=/dev/../etc/passwd`
- `device=/dev/nvme0n1/../../etc/shadow`
- `device=/etc/passwd`
- `device=/dev/`
- `device=/dev/does-not-exist`
- a missing `device` query parameter

## Privacy policy

The default document is deliberately narrower than `smartctl` output:

- **Models are exposed** (bounded, sanitized); they are already present in
  sysfs and are useful for identifying a device class.
- **Serials are not exposed by default.** A serial is stable hardware
  identifying material. Callers must opt in with `?serials=true`; the value is
  then sanitized and capped at 64 characters. The response records
  `serials_included: true` so the policy is explicit.
- **Arbitrary SMART attributes are not exposed by default.** The default
  schema exposes only health, temperature, NVMe wear/spare/media errors, and
  the three ATA defect counters. Callers must opt in with
  `?attributes=true` to receive the normalized `ata_smart_attributes.table`
  rows (id, name, value, worst, threshold, raw value/text), capped at 64 rows.
  There is no way to request the raw smartctl JSON.
- **No raw command output, stderr, environment, or command line is returned.**
  Error messages are control-character sanitized, trimmed, and capped.
- **No identifiers are logged** by these paths; only bounded error strings
  reach the response.

The controller proxy does not add serials or attributes; it forwards the
caller's explicit flags.

## Typed errors

| Status | Code | Where |
|---|---|---|
| 503 | `storage.block-unavailable` | `/storage/health` when `/sys/block` cannot be read |
| 503 | `storage.smart-unavailable` | `/storage/smart` when smartctl is missing, times out, fails, or returns no usable SMART data; also nested per device on `/storage/health` |
| 503 | `storage.md-unavailable` | nested in `/storage/health` when `/proc/mdstat` cannot be read |
| 400 | `storage.invalid-device` | `/storage/smart` for a missing, non-`/dev`, traversal, or nonexistent device |

The error body keeps the existing host-surface shape:

```json
{"ok": false, "available": false, "code": "storage.smart-unavailable", "error": "..."}
```

The controller's `_proxy_node_surface` already preserves `storage.*` typed
errors with their status, so `400` and `503` survive the proxy unchanged.

## Controller proxies

| Controller route | Node path |
|---|---|
| `GET /api/v1/nodes/{node_id}/storage/health?serials=&attributes=` | `/storage/health?...` |
| `GET /api/v1/nodes/{node_id}/storage/smart?device=&serials=&attributes=` | `/storage/smart?device=...` |

The smart device query is URL-encoded (`quote(device, safe="")`) and the node
bearer token is forwarded. The node remains responsible for validation,
bounds, and privacy defaults.

## Tests

Rust (`cargo test -p ozma-node --lib host_surfaces::`):

- NVMe JSON normalization (health, temperature, wear, spare, media errors,
  power-on hours, protocol) and serial opt-in;
- failing SATA JSON (overall failure, reallocated/pending/uncorrectable
  counts, bounded attribute rows) and attribute opt-in;
- smartctl error-message documents become typed unavailability;
- mdstat healthy and degraded arrays, level/state/members/slot map/sync/size;
- malformed mdstat lines are ignored without panicking;
- `mdadm --detail --scan` UUID/metadata parsing;
- traversal and missing-device validation (including `/dev/../etc/passwd`,
  nested `..`, non-`/dev`, empty names, and a nonexistent device);
- a 40-device fixture proves the 32-device bound, truncation, and default
  serial/attribute redaction;
- per-device SMART/md unavailability keeps the health document a `200` with
  typed nested codes;
- malformed smartctl output and a missing smartctl are typed
  `storage.smart-unavailable`;
- router tests cover both versioned aliases, `?serials=true`, `?attributes=true`,
  missing/invalid device queries, and the typed `400`/`503` bodies.

Controller
(`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- both health/smart proxies are in the bearer-token matrix;
- the health proxy forwards `serials=true&attributes=true`;
- the smart proxy URL-encodes the device and forwards the flag;
- typed `storage.invalid-device` (400) and `storage.smart-unavailable` (503)
  are preserved without leaking the token.

## Live evidence

On the reference host (`smartctl 7.5`, NVMe, no ZFS, no `/proc/mdstat`):

- `sudo smartctl -j -H -A /dev/nvme0n1` reports
  `smart_status.passed: true`, `temperature.current: 49`, and
  `percentage_used: 21`;
- `/sys/block` contains `nvme0n1` and `zram0`; `zram0` is filtered, so
  `/storage/health` reports exactly one device;
- `/proc/mdstat` is absent, so the md section is a typed
  `storage.md-unavailable` while the health document remains `200`;
- the non-root live test proves the honest permission path: when `smartctl`
  cannot open the NVMe device it returns `storage.smart-unavailable` rather
  than inventing health;
- running the same live test with root exercises the successful smartctl path
  and asserts a real temperature/wear value with serials omitted.

## Remaining gaps

- **No SMART self-tests or scheduling.** Short/long tests, test logs, and
  smartd integration remain out of scope.
- **No md/ZFS administration.** No assemble/start/stop/add/remove, no scrub,
  no replacement workflow, no ZFS pool/vdev reporting.
- **No serial by default.** Dashboards that need asset tags must opt in; the
  default remains non-identifying.
- **No device-type override.** Smartctl is run without `-d`; unusual USB/SAS
  bridges may need an explicit type and currently report typed unavailability.
- **Bounded, sequential smartctl calls.** Health covers at most 32 devices and
  runs one smartctl process at a time. A host with dozens of slow devices can
  take several seconds.
- **Permissions are the node's.** The feature does not use sudo or elevate
  privileges; an unprivileged node reports typed unavailability per device.
- **No history.** Only the current sample is returned; wear/capacity trends
  remain unported.
