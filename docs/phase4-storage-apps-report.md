# Phase 4 — storage, application, and self-management surfaces

Status: **read-only observability implemented; privileged storage
administration and process enforcement remain out of scope.**

`ozma-node` now serves bounded, honest host facts that the deleted Python
agent exposed through `agent/storage_manager.py`, `agent/app_monitor.py`, and
`node/self_management.py`. The routes are deliberately observational: they do
not mount, format, scrub, kill, or reconfigure anything.

Read-only SMART and mdraid visibility lives next to these routes and is
documented in [`phase4-storage-health-report.md`](phase4-storage-health-report.md).

## Endpoints

| Route | Auth | Source | Bound |
|---|---|---|---|
| `GET /storage` (alias `GET /api/v1/storage`) | node bearer guard | `/proc/mounts` + `statvfs` | 64 filesystems |
| `GET /storage/health` (alias `GET /api/v1/storage/health`) | node bearer guard | `/sys/block`, `smartctl -j -H -A`, `/proc/mdstat`, optional `mdadm --detail --scan` | 32 devices, 32 arrays, 64 members/array |
| `GET /storage/smart?device=/dev/nvme0n1` (alias `GET /api/v1/storage/smart`) | node bearer guard | `/sys/block` + one bounded `smartctl -j -H -A` | one device |
| `GET /apps?limit=N` (alias `GET /api/v1/apps`) | node bearer guard | numeric `/proc/<pid>/{comm,stat,status}`, `/proc/uptime`, `/etc/passwd` | `limit` 1–100, default 20 |
| `GET /self` (alias `GET /api/v1/self`) | node bearer guard | `/proc/self/{stat,statm,cgroup}`, `/proc/uptime`, crate metadata | fixed document |

The routes are mounted in `controller_compat::compat_router` next to
`/hardware`, so they use the existing node bearer-token guard. They are not
merged into `public_router`; process owners and mount layout are host
inventory, and the controller proxy sends the node token.

### `GET /storage`

Only mounts whose source starts with `/dev/` are reported. Pseudo/network
filesystems (`proc`, `tmpfs`, `overlay`, `sysfs`, `nfs`, …) are filtered out,
even when their source happens to look like a device. Mount-table octal
escapes (`\040`, `\011`, `\012`, `\134`) are decoded, control characters are
replaced, and the list is capped at 64 entries.

Each entry contains:

```json
{
  "device": "/dev/nvme0n1p2",
  "mount": "/",
  "fs": "ext4",
  "total_bytes": 1000000000000,
  "used_bytes": 420000000000,
  "free_bytes": 580000000000,
  "percent_used": 42.0
}
```

`used_bytes = total_bytes - free_bytes` and `free_bytes` uses `statvfs.f_bfree`
(including root-reserved space), so the three capacity fields stay consistent.
The document also reports `count`, `matched`, `limit`, `truncated`, and
`skipped` (matched mounts whose `statvfs` failed, for example a stale device).

An unreadable mount table is a typed `503`:

```json
{"ok": false, "available": false, "code": "storage.proc-unavailable", "error": "..."}
```

`/storage` itself does not open a device or make a SMART/IOCTL call, and no
ZFS/md/multipath administration route is added. SMART and mdraid visibility
is the separate read-only `/storage/health` and `/storage/smart` surface
described below.

### `GET /storage/health`

Per-device block inventory and health, bounded to 32 devices:

- model, size, rotational flag, and (only with `?serials=true`) serial;
- normalized SMART availability/health, temperature, NVMe percentage-used and
  spare, and SATA reallocated/pending/uncorrectable counts;
- md array state, level, members, slot map, degraded flag, sync progress, and
  optional UUID/metadata.

SMART and mdraid are optional enrichment. A missing `smartctl` or a host
without `/proc/mdstat` keeps the health document a `200` and reports typed
`storage.smart-unavailable` / `storage.md-unavailable` sections. An unreadable
`/sys/block` is a typed `503 storage.block-unavailable`.

```json
{
  "ok": true,
  "count": 1,
  "limit": 32,
  "truncated": false,
  "serials_included": false,
  "attributes_included": false,
  "smart": {"available": true},
  "md": {"available": false, "code": "storage.md-unavailable", "arrays": []},
  "unavailable": ["md"],
  "devices": [
    {
      "name": "nvme0n1",
      "device": "/dev/nvme0n1",
      "model": "Fanxiang S880E 1TB",
      "size_bytes": 1000204886016,
      "rotational": false,
      "smart": {"available": true, "passed": true, "temperature_c": 49, "percentage_used": 21}
    }
  ]
}
```

Serials and arbitrary SMART attributes are **not** returned by default. Both
are explicit opt-ins (`?serials=true`, `?attributes=true`) and remain bounded.
See the storage-health report for the full privacy policy, mdstat grammar, and
errors.

### `GET /storage/smart?device=/dev/nvme0n1`

One validated block device's normalized SMART document. `device` must be
`/dev/<basename>`, must exist under `/sys/block` or `/dev`, and is rejected
with a typed `400 storage.invalid-device` for traversal, slashes, control
characters, or a missing device. Smartctl is run with a 5 s deadline and a
1 MiB stdout bound; raw smartctl JSON is never returned. A missing tool or
unusable device is a typed `503 storage.smart-unavailable`.

### `GET /apps?limit=N`

The process list is bounded twice: the scan stops after 16,384 `/proc`
entries, and at most 100 processes are returned. `limit` is clamped to
`1..=100`; the default is 20. Entries contain only:

- `pid`
- `name` (kernel `comm`, control characters replaced, 64 characters max)
- `cpu_percent` and `cpu_window`
- `rss_bytes`
- `user` (passwd name, `uid:<n>` when passwd has no entry, or `unknown`)
- `uid` (numeric, or `null` when `/proc/<pid>/status` was unreadable)

**`cmdline` is never read or returned.** The test fixtures include a
`/proc/<pid>/cmdline` containing a password, and assert the serialized
document never contains it. The route also never reads another process's
environment.

CPU uses two honest windows:

- first request: lifetime average from `utime + stime` over `/proc/uptime`,
  labelled `since-boot`;
- later requests at least 50 ms apart: delta against the previous sample,
  labelled `since-last-sample`.

`cpu_percent = 100` is one full core; a multi-threaded process may legitimately
exceed 100. If neither window is measurable, the field is `null` rather than a
fake zero. Processes are sorted by CPU descending, then RSS descending, then
pid. The document includes `count`, `matched`, `limit`, `truncated`,
`scan_truncated`, `unreadable`, `cpu_window`, and a summary of returned
process count, distinct users, and total RSS.

Unreadable procfs is a typed `503` (`apps.proc-unavailable`). Individual
processes that disappear mid-scan are skipped and counted.

### `GET /self`

The self report names this node process only:

```json
{
  "ok": true,
  "available": true,
  "node_id": "node-...",
  "pid": 1234,
  "version": "0.1.0",
  "profile": "hardware",
  "uptime_seconds": 812.4,
  "host_uptime_seconds": 912.4,
  "memory": {"virtual_bytes": 123456789, "rss_bytes": 45678901},
  "unit": "ozma-node.service",
  "unit_source": "cgroup",
  "restart_count": null,
  "restart_count_source": null,
  "systemd": true,
  "unavailable": ["restart_count"]
}
```

- `pid` is `std::process::id()`.
- Process uptime is derived from `/proc/uptime` and this process's procfs
  `starttime`; memory comes from `/proc/self/statm`.
- `unit` is `OZMA_UNIT` when the supervisor exports it, otherwise the
  `.service`/`.scope` name visible in `/proc/self/cgroup`.
- `restart_count` is populated **only** from `OZMA_RESTART_COUNT`.
  systemd does not export `NRestarts` to a service, so the field is `null`
  when no supervisor supplies the value. It is never inferred from pid 1,
  process age, or logs.
- `systemd` is true only when `INVOCATION_ID`/`JOURNAL_STREAM` or a cgroup
  unit marker is visible.

An unreadable `/proc/self/stat` is a typed `503`
(`self.proc-unavailable`). Missing optional facts are `null` and named in
`unavailable`.

## Availability advertisement

`GET /api/v1/connection` (and its `/node/status` alias) gains a
`host_surfaces` object:

```json
"host_surfaces": {
  "storage": {
    "supported": true,
    "path": "/storage",
    "available": true,
    "guarded": true,
    "reason": "Reads mounted /dev block filesystems and statvfs capacity; read-only, no secrets, at most 64 entries.",
    "limit": 64
  },
  "health": {
    "supported": true,
    "path": "/storage/health",
    "available": true,
    "guarded": true,
    "reason": "Reports bounded /sys/block inventory, normalized SMART health, and /proc/mdstat arrays; read-only, serials opt-in, at most 32 devices.",
    "limit": 32
  },
  "smart": {
    "supported": true,
    "path": "/storage/smart",
    "available": true,
    "guarded": true,
    "reason": "Runs bounded smartctl -j -H -A for one validated /dev block device; no tests, no raw output, serials opt-in.",
    "limit": 1
  },
  "apps": {"supported": true, "path": "/apps", "available": true, "guarded": true, "limit": 100, "...": "..."},
  "self": {"supported": true, "path": "/self", "available": true, "guarded": true, "...": "..."}
}
```

Availability is captured once at startup with cheap procfs `open`/`read_dir`
checks. Building the connection document does **not** enumerate mounts, run
`statvfs`, walk processes, read `/etc/passwd`, or invoke a subprocess. The
document contains no mount list, process list, pid, RSS, username, or unit;
those are only returned by the guarded routes.

If a source disappears later, the route answers its typed `503` while the
advertisement continues to say which source was available at startup.

## Controller proxies

The controller now exposes minimal pass-throughs next to its hardware proxies:

| Controller route | Node path |
|---|---|
| `GET /api/v1/nodes/{node_id}/storage` | `/storage` |
| `GET /api/v1/nodes/{node_id}/storage/health?serials=&attributes=` | `/storage/health?...` |
| `GET /api/v1/nodes/{node_id}/storage/smart?device=&serials=&attributes=` | `/storage/smart?device=...` |
| `GET /api/v1/nodes/{node_id}/apps?limit=N` | `/apps?limit=N` |
| `GET /api/v1/nodes/{node_id}/self` | `/self` |

They use `_proxy_node_surface`, so a node's typed error (`storage.*`,
`apps.*`, `self.*`) survives as JSON with its status, and the node bearer
token is forwarded. The `limit` query is passed through; the node remains
responsible for clamping it to 100. The health/smart proxies forward the
explicit privacy flags and URL-encode `device`; the node still validates the
device and applies its own bounds.

No Python agent route used these paths: `agent/storage_manager.py` and
`agent/app_monitor.py` were libraries, and `node/self_management.py` served its
report at `GET /node/status`. The Rust node keeps `/node/status` as the
connection summary (plus the `host_surfaces` advertisement) and adds `/self`
for the bounded process report. `controller/mesh_intelligence.py` still reads
the Python self-report shape (`cpu`, `memory`, `storage`, `network`, `wifi`)
from `/node/status`; those fields are absent on a Rust node, so the mesh
dashboard's derived CPU/memory/storage/network values remain zero until that
consumer is taught to use `/self` or a nested self document. This is recorded
as a remaining gap rather than faking Python-shaped fields in the connection
document.

## Tests

Rust (`cargo test -p ozma-node --lib host_surfaces::`):

- mount parsing/filtering, octal unescaping, control-character sanitization;
- empty mount table, capacity math, `statvfs` skip counting, bounded output
  and `truncated` with more than 64 block mounts;
- typed `storage.proc-unavailable` for an unreadable mount table;
- app parsing from synthetic `/proc` trees (names with spaces and `)`, RSS
  pages, uid/passwd resolution, unknown uid, CPU percentages), sorting,
  `limit` clamps (0 → 1, huge → 100), truncation, skip counting, and typed
  `apps.proc-unavailable`;
- first-sample `since-boot` and later `since-last-sample` CPU windows;
- serialized app/storage/self documents contain no `cmdline`, no cmdline
  contents, no `/etc/passwd` contents, and no environment values;
- self pid/version/profile/uptime/RSS/cgroup-unit/restart-count honesty,
  including `restart_count: null` and `unavailable` when the supervisor did
  not export it;
- an HTTP router test checks the JSON shape, bounded `/apps?limit=1`, and the
  typed `503` body;
- a live Linux host test calls `/storage`, `/apps`, and `/self` collectors
  directly and checks bounds, capacity consistency, memory presence, and the
  absence of `cmdline`;
- storage-health tests cover NVMe/SATA SMART normalization, serial/attribute
  redaction and opt-in, mdstat healthy/degraded/malformed parsing,
  `mdadm --detail --scan`, traversal/missing-device validation, the 32-device
  bound, and typed `storage.block-unavailable`, `storage.smart-unavailable`,
  `storage.md-unavailable`, and `storage.invalid-device` responses;
- a live Linux test exercises `/storage/health` and `/storage/smart` against
  the host NVMe and asserts bounds, default serial redaction, and honest typed
  unavailability when the process lacks device permissions.

Controller
(`python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`):

- `/storage`, `/storage/health`, `/storage/smart`, `/apps`, and `/self` are in
  the bearer-token proxy matrix;
- `test_apps_proxy_forwards_limit_to_the_node` proves the `limit` query and
  token reach the node;
- the health/smart proxy test proves flag forwarding, device URL-encoding, and
  preservation of typed `400 storage.invalid-device` /
  `503 storage.smart-unavailable` bodies without a token leak.

## Live evidence

The live Linux test exercises the real `/proc/mounts` + `statvfs` path and a
real `/proc` process scan. On a container host with an `overlay` root, the
storage report is honestly empty (no `/dev`-backed mount); on a host with
`/dev/*` mounts it returns those capacities up to 64 entries. `/apps` returns
only the requested window; `/self` always resolves this test process's pid and
memory.

The reference host has `smartctl 7.5` and one physical NVMe device
(`/dev/nvme0n1`); `/sys/block` also contains `zram0`, which the health
inventory filters out. `/proc/mdstat` is absent, so the live health document
reports a typed `storage.md-unavailable` while still returning the NVMe
inventory and SMART data. As a non-root process, smartctl cannot open the
device and the same route reports per-device `storage.smart-unavailable`
instead of inventing health; as root the live test asserts the real
temperature/percentage-used values with serials omitted.

## Remaining gaps

- **No storage administration.** ZFS pools/vdevs, md assembly/start/stop/add/
  remove, multipath, enclosure LEDs, SMART self-tests/scheduling, replication,
  iSCSI, ACLs, secure wipe, capacity history, and S3 gateway helpers from
  `storage_manager.py`/`storage_admin.py` are not ported. `/storage/health`
  and `/storage/smart` are observational only. `/backup/*` remains a typed
  `501`.
- **No app enforcement.** `app_monitor.py` blocked/killed processes, enforced
  whitelist/blacklist/kiosk policy, timers/breaks, screen locking, and usage
  reporting. The node only lists processes; it never reads `cmdline`, kills,
  or reports usage to the controller.
- **Self-management is a subset.** Network throughput/errors/drops, Wi-Fi
  signal/channel/noise, link speed, load average, CPU temperature, swap, USB
  gadget speed, and controller RTT from `node/self_management.py` are not in
  `/self`. The existing `/agent/metrics`, `/hardware`, and `/usb` routes cover
  parts of that vocabulary.
- **`/node/status` consumer mismatch.** The Python node returned the
  self-management report at `/node/status`; the Rust node returns the
  connection document plus `host_surfaces`. `mesh_intelligence` therefore sees
  defaults for the Python-only keys. Aligning that consumer (read `/self` or a
  nested `self_management`) is a controller follow-up.
- **No privileged mount/devices.** The route cannot see a block device that the
  node process is not allowed to `statvfs`, and it deliberately does not use
  elevated privileges to inspect one. `/storage/smart` inherits the same
  policy: an unprivileged node returns typed `storage.smart-unavailable`
  rather than invoking sudo.
