# Phase 4 — FunctionFS SCSI Media Path Report

Status: implemented and green (no commit). Scope: `ozma-usb/` and
`ozma-node/` USB/media wiring plus this report. No changes were made to
`docs/parity-matrix.md` or `docs/plan-execution-status.md`.

## Summary

`MassStorageDevice` previously decoded SCSI BBB CBWs but nothing drove it from
a FunctionFS endpoint. This phase adds an injectable bulk transport and a
bounded command loop, then exposes it through node routes:

- `MassStorageLoop` reads a CBW from bulk OUT, reads an OUT data phase when
  declared, dispatches `MassStorageDevice::execute_cbw`, writes IN data, and
  writes the 13-byte CSW.
- `FunctionFsBulkTransport` adapts an enabled `FunctionFsDaemonCore`.
- `GET/POST /usb/media`, `POST /usb/media/stop`, and `GET /usb/hotplug`
  start/stop a userspace export and report state, capacity, overlay status,
  counters, and bounded transitions.
- `--usb-media-export` auto-starts the same export at node startup from the
  `--usb-media-export*`/`--usb-media-restart*` options. The startup state is
  typed `not-configured`/`configured`/`running`/`failed`; a failure never
  crashes the node.
- `FunctionFsMediaDaemon` is the durable owner of a mounted FunctionFS
  function: it writes `ep0` descriptors, waits for a bounded `ENABLE`, attaches
  a `PollingFileBulkTransport`, services the SCSI loop continuously, detaches on
  `DISABLE`/`UNBIND`, applies the restart policy, and flushes on shutdown.
  `--usb-functionfs-mount` opts a node into this daemon.
- `FunctionFsHandoff` sequences the real mount-before-bind contract: create the
  gadget/function unbound, mount FunctionFS at the configured path, start the
  daemon, wait for its `ep0` writes, then bind the UDC. Each phase is typed and
  a failure rolls back the steps already taken. The operator no longer needs to
  pre-mount FunctionFS.
- `MediaRestartPolicy` bounds recovery after a bulk-transport error or an
  observed FunctionFS unbind, with capped attempts/backoff and typed last error.
  Counters stay visible across a rebuild and the persisted overlay is reloaded
  according to policy.
- `running` is only reported after a real command cycle completed over an
  attached transport. A configured or auto-started export is `configured`
  (or daemon `waiting-for-enable`/`enabled`); an unattached export uses
  `IdleBulkTransport` and cannot claim a transfer.

## 1. Bulk transport and event loop

New module: `ozma-usb/src/mass_storage_loop.rs`.

### Trait contract

```rust
pub trait MassStorageBulkTransport {
    fn read_out(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError>;
    fn write_in(&mut self, data: &[u8], timeout_ms: u64) -> Result<usize, UsbError>;
    fn flush_in(&mut self, timeout_ms: u64) -> Result<(), UsbError> { Ok(()) }
}
```

- A transport may return short reads/writes; the loop assembles CBW and data
  phases and retries writes.
- A stalled endpoint must return
  `UsbError::MassStorageEndpointTimeout { operation, timeout_ms }`.
- A zero-byte transfer or an impossible length is a typed
  `UsbError::MassStorageTransport`.
- `Box<T>` and `Box<dyn MassStorageBulkTransport + Send>` are supported through
  a blanket implementation. The node alias is `BoxedMassStorageTransport`.

### Loop behavior

`MassStorageLoopConfig` bounds and validates:

| Field | Default | Bound |
|---|---|---|
| `transfer_timeout_ms` | 5000 | `1..=60000` |
| `max_transfer_bytes` | 1 MiB | sector-aligned `512..=SCSI_MAX_TRANSFER_BYTES` |
| `max_commands` | `None` | optional positive limit for `run` |

`MassStorageLoop::step()`:

1. Reads exactly 31 CBW bytes from bulk OUT.
2. Parses `ScsiCommandBlock`; malformed CBWs are typed `protocol-error`.
3. Rejects a declared data phase above `max_transfer_bytes`
   (`mass-storage-buffer-too-large`) before reading it.
4. Reads the declared OUT data phase (currently `WRITE(10)` uses it).
5. Dispatches to `MassStorageDevice::execute_cbw`.
6. Writes any IN data, then the CSW, then flushes the IN endpoint.
7. Counts the command and marks the loop `running`.

SCSI failures (read-only write, LBA out of range, unsupported opcode) are
normal CSW check conditions with sense data, not transport errors. Transport
and timeout failures propagate as typed errors. `stop()` clears `running` and
applies the device's `MassStoragePersistPolicy`.

`MassStorageLoopStats` reports `commands`, `cbws`, `csws`, data bytes,
read/write/flush command counts, check conditions, phase errors, transport
errors, timeouts, and bound errors.

### Transports

- `MemoryBulkTransport`: default-test fake with separate host→device and
  device→host queues, optional short chunks, and injected timeout/I/O faults.
- `IdleBulkTransport`: no endpoint attached; returns
  `mass-storage-not-attached`.
- `FileBulkTransport`: owns two explicitly supplied endpoint files, so an
  export can keep its transport for its lifetime.
- `FunctionFsBulkTransport`: borrows a `FunctionFsDaemonCore` and one OUT plus
  one IN address. Reads are clamped to the FunctionFS 64 KiB per-transfer
  limit; writes are chunked to it. It is tested through FunctionFS loopback
  endpoints only.

## 1a. Background media daemon

New module: `ozma-usb/src/media_daemon.rs` (exported as `FunctionFsMediaDaemon`
and related types). It owns the long-lived FunctionFS + SCSI state:

| Step | Behaviour |
|---|---|
| Start | `write_descriptors()` writes the v2 blob and string table to `ep0`; state becomes `waiting-for-enable`. |
| Wait | `PollingFileEp0Io` makes `ep0` nonblocking and waits with `poll(2)` up to `MediaDaemonConfig::event_timeout_ms`; `MemoryEp0` returns immediately and the worker uses `idle_poll_ms`. No busy loop. |
| ENABLE | The core opens no data endpoints in external-endpoint mode; the daemon attaches a `PollingFileBulkTransport` and a rebuilt `MassStorageLoop`. State `enabled`, never `running` yet. |
| Serve | One `step()` per tick; each transfer uses the SCSI loop polling deadline. `running` becomes true only after a CSW is written. |
| Idle timeout | A timeout before the CBW counter advances is normal idleness: the transport stays attached and no restart is scheduled. |
| Transport failure | A timeout after a CBW started (or any non-timeout transport error) marks `failed`, detaches/stops the runner, commits the overlay, and schedules the next bounded attempt. |
| DISABLE | Detach and stop the runner (persistence policy applied); state `waiting-for-enable`. |
| UNBIND | Detach, record an unbind, schedule the restart; the next ENABLE attaches a device rebuilt with `reload_overlay` per policy. |
| Stop | `shutdown()` stops the runner, closes `ep0`/endpoints, applies the persistence policy, and publishes `stopped`. `MediaDaemonHandle::shutdown_timeout()` bounds the join; `shutdown()` uses the same 10 s default. A FunctionFS node unbinds the UDC once *before* signalling the daemon so a pending endpoint read is woken, then unmounts and removes the function/gadget only after the worker has flushed and exited. |

`MediaDaemonStatus` exposes `state`, `live`/`enabled`/`attached`/`running`,
preserved `MassStorageLoopStats`, `FunctionFsDaemonStats`, lifecycle counters
(events, binds/unbinds, enables/disables, setups, attaches/detaches/restarts),
restart attempts/exhaustion/backoff, and a path-free last error. Device and
transport factories are injected; `FunctionFsMediaDaemon::file` is the real
explicit-mount constructor and still never mounts FunctionFS.

Node selection: `--usb-functionfs-mount PATH` requires `--usb-gadget`,
`--usb-vendor`, and `--usb-media-export`. The node creates the `ffs.vendor0`
function and mounts FunctionFS at `PATH` itself (root required), then builds the
same bulk-OUT-1/bulk-IN-2 FunctionFS model, the polling transport factory, and
the daemon, waits up to 5 s for the daemon's `ep0` writes, and binds the UDC.
`/usb/media` reports `transport.kind = "polling-file"` and the daemon counters;
a transfer is still required before `running` is true. The node does not use the
blocking `FileBulkTransport` for this path.

The ordered handoff is in `ozma-usb/src/functionfs_handoff.rs`:

| Phase | Work |
|---|---|
| `prepared` | `create_gadget` created the full composite gadget and linked `ffs.<instance>`; UDC empty. |
| `mounted` | The configured mount was validated, or `CommandFunctionFsMounter` ran `mount -t functionfs <instance> <path>`. |
| `daemon_started` | Caller's daemon wrote the descriptor/string blobs; bind is still rejected before this. |
| `bound` | UDC written exactly once. |
| `unbound` | UDC cleared once with `"\n"` (ConfigFS ignores a zero-byte write). |
| `torn_down` | Owned mount unmounted first, then function/gadget removed. |

Failure at prepare/mount/bind rolls back the gadget, mount, and UDC in reverse
order and returns the original typed error. Cleanup leaves a caller-owned
pre-existing mount alone. The FunctionFS interface advertises Mass Storage /
SCSI transparent / Bulk-Only (`0x08/0x06/0x50`) for this media path so the host
binds `usb-storage` automatically.

## 2. Node routes

`ozma-node/src/controller_compat.rs` adds:

| Route | Purpose |
|---|---|
| `GET /usb/media` | State, capacity, overlay/persistence summary, counters, typed last error. |
| `POST /usb/media/start` | Build a userspace SCSI export from an existing image. |
| `POST /usb/media/stop` | Stop the export and apply the overlay persistence policy. |
| `GET /usb/hotplug` | Bounded poll view of `export_start`, `export_stop`, `attach`, `detach`, `unbind`. |
| `GET /usb` | Now includes a nested `media_export` object with the same state. |

Start request fields: `path`, optional `function`, `format`
(`raw`/`fat32`/`iso9660`), `read_only`, `persist_path`, `persist_on_flush`,
`persist_on_stop`, `reload`, `max_transfer_bytes`, `timeout_ms`.

Node startup options mirror those fields:

| Option | Purpose |
|---|---|
| `--usb-media-export PATH` | Opt in to the userspace SCSI export at startup (distinct from the ConfigFS `--usb-media` LUN). |
| `--usb-media-export-format FORMAT` | `raw` (default), `fat32`, or `iso9660`. |
| `--usb-media-export-read-write` | Use the bounded copy-on-write overlay for raw media. |
| `--usb-media-export-persist PATH` | Absolute overlay persistence file. |
| `--usb-media-export-no-persist-on-flush` / `-no-persist-on-stop` | Opt out of the default flush/stop commits. |
| `--usb-media-export-reload` | Load the existing overlay before serving. |
| `--usb-media-export-timeout-ms`, `--usb-media-export-max-transfer-bytes` | Endpoint/data-phase bounds. |
| `--usb-media-restart-max-attempts` (default 3) | Bounded restart cap; `0` disables restart. |
| `--usb-media-restart-backoff-ms` / `--usb-media-restart-max-backoff-ms` (250/4000) | Capped exponential backoff. |
| `--usb-media-restart-no-overlay-reload` | Rebuild from the base image instead of the persisted overlay. |
| `--usb-functionfs-mount PATH` | Opt into the background daemon and the node-owned mount-before-bind handoff; requires `--usb-gadget --usb-vendor --usb-media-export`. The node mounts this path itself (root) after creating the `ffs` function. |

Semantics:

- Raw media defaults to read-only. `read_only: false` opts into the bounded
  copy-on-write overlay; the base file is never opened for writing.
- FAT32 and ISO 9660 are always read-only and reject `read_only: false`.
  They are probed with `MassStorageImage::validate_for_attach` before serving.
- `persist_path` must be absolute and distinct from the base. `reload: true`
  loads an existing checksummed overlay with `MassStorageDevice::load_writable_file`.
- Media state is `configured` immediately after start or auto-start. It becomes
  `running` only after `MassStorageLoop::step()` writes a CSW. A transport
  failure marks it `failed` with a path-free typed `last_error`; stop marks it
  `stopped`.
- Overlay status is path-free: geometry, allocation, dirty flag, whether a
  persistence file exists, its size, and the flush/stop policy.
- `GET /usb/hotplug?since=<sequence>&limit=<n>` is capped at 64 retained
  events and never synthesises kernel events. Attach/detach/export events are
  recorded only after the corresponding operation succeeds. Auto-start is
  `export_start` state `auto-start`; restart/rebind attempts are `restart` or
  `rebind` with `configured`/`failed`. `unbind` is recorded only when a live
  gadget snapshot transitions from bound to unbound.
- Auto-start is opt-in. Without `--usb-media-export`, `/usb` reports
  `media_export.state: "not-configured"` and no userspace export is created.
  A failed auto-start records the typed reason and leaves `running: false`.
- On SIGINT/SIGTERM the node unbinds the UDC once, stops the daemon with a
  bounded join, flushes the overlay according to `persist_on_stop`, unmounts
  FunctionFS, and removes the function/gadget. No second unbind or manual
  unmount is required.

## 3. Restart supervision

`ozma-usb/src/media_restart.rs` defines the policy/tracker and
`ozma-node/src/controller_compat.rs` applies it:

- `MediaRestartPolicy` caps attempts at `0..=16` (default 3), computes capped
  exponential backoff (default 250 ms → 4 s), and optionally reloads the
  persisted overlay.
- `MediaRestartTracker::record_failure` returns the next attempt/backoff or
  `None` once the cap is exhausted; `record_success` resets the streak after a
  real CSW. An exhausted export stays `failed`, so there is no infinite crash
  loop.
- A supervised rebuild preserves `MassStorageLoopStats` and the typed last
  error. If `reload_overlay` is set and a persistence path exists, the device
  commits the overlay before rebuilding and loads it afterwards; otherwise it
  restarts from the base image.
- The same path runs when `/usb` observes a gadget `bound → unbound`
  transition, recording `unbind` then `rebind` before marking the export
  `configured` (never `running`).
- Shutdown calls `MassStorageLoop::stop`, which commits the overlay when
  `persist_on_stop` is set, then records `export_stop` state `shutdown`.

## 4. Tests

New library suite: `ozma-usb/tests/mass_storage_loop.rs` (10 tests):

- CBW → data → CSW round trip over fake endpoints.
- Write/read/flush through the overlay, persistence file, base immutability,
  and reload through `MassStorageDevice::load_writable_file`.
- Read-only write rejection with `DATA PROTECT` sense.
- Oversized data-phase rejection with a typed bound.
- Stalled read/write timeout and injected I/O failure.
- Short-transfer assembly with 7-byte reads / 5-byte writes.
- Idle transport never claims `running`.
- Stop commits the overlay according to policy.
- Owned `FileBulkTransport` endpoint files drive the loop.
- `FunctionFsBulkTransport` drives the SCSI loop through enabled FunctionFS
  loopback endpoints and observes daemon bytes read/written.

New `ozma-usb` daemon tests in `media_daemon::tests` (7 tests, in-memory
endpoints/fakes only):

- enable → attach → serve one CSW → disable → detach, with no transfer claimed
  before a host CBW;
- a mid-command timeout triggers a bounded restart and a later real CBW recovers
  `running` and resets the failure streak;
- an unbind detaches, rebuilds with overlay reload, waits for the next ENABLE,
  and attaches with the reloaded device;
- shutdown stops the runner, commits the persistent overlay, leaves the base
  image untouched, and flushes the endpoint;
- a spawned worker exits within its bounded shutdown even though no UNBIND event
  or second external UDC unbind was supplied (`counters.unbinds == 0`);
- invalid event/idle bounds are typed;
- bulk endpoint selection requires one non-zero OUT and one IN endpoint.

New `ozma-usb` handoff tests in `tests/functionfs_handoff.rs` (7 tests):

- prepare → mount → daemon-started → bind ordering, with bind rejected before
  the mount/daemon phase;
- one unbind on shutdown, a repeated `unbind_once` no-op, and idempotent cleanup
  (`unbind_calls == 1` is the single-unbind regression);
- mount failure rolls the created function back and returns the typed error;
- bind failure unbinds/removes the function and unmounts;
- preparing over an existing gadget is typed `gadget-exists` and leaves it alone;
- a missing mount without a mounter is typed and rolls back;
- a caller-supplied mount is not unmounted by cleanup.

`ozma-usb/tests/configfs_controller.rs` also asserts that unbind writes `"\n"`
(not a zero-byte no-op) to the UDC attribute.

New `ozma-node` tests (2):

- `/usb` defaults `usbip` to `not-configured`, adapter `kernel-required`,
  `kernel_required: true`, and zero data sessions;
- media auto-start with `functionfs_mount` installs a prepared/mounted
  `FunctionFsHandoff`, starts the polling daemon, observes its `ep0` writes
  before binding, reaches `waiting-for-enable` without claiming `running`,
  reports `transport.kind: "polling-file"`, and stops through
  `POST /usb/media/stop` with exactly one fake-controller unbind, no leaked
  gadget, and no leaked mount.

New `ozma-usb` restart-policy tests in `media_restart::tests` (5 tests):

- Default policy validates and keeps overlay reload enabled.
- Backoff doubles and clamps at the ceiling.
- Attempts stop exactly at the cap and report exhaustion.
- A zero cap never restarts; a successful transfer resets the streak.
- Invalid attempt/backoff bounds return a typed error.

New node tests in `controller_compat::tests` (11 media tests total; 5 added):

- Auto-start not configured (`not-configured`) and configured (`configured`,
  `auto_start: true`) with `running: false`, zero commands, and an
  `export_start` hotplug event state `auto-start`.
- Auto-start failure on a missing image is a typed `failed` state with
  `media-not-found`, no path echoed, and `running: false`.
- Transport timeout → `failed`, bounded restart attempt 1 with 25 ms backoff,
  counters preserved, then a healthy transfer returns to `running` and clears
  the failure streak.
- Repeated failures hit the attempt cap, stay `failed`/`exhausted`, and a
  further restart request does not start a new loop.
- Writing through the overlay, restarting with reload, verifying the persisted
  sector, then a second unflushed write is committed by shutdown; the base image
  remains untouched.
- FunctionFS unbind observed from `/usb` records `unbind`/`rebind`, reloads the
  export, and leaves it `configured`, not `running`.
- Existing configured/running/stopped, overlay persistence/reload, bad request,
  duplicate start, hotplug bounds, type-503, and no-kernel-LUN tests still pass.

Verification run:

```text
cargo test -p ozma-usb -p ozma-node --lib
  # ozma-node: 551 passed, 4 ignored
  # ozma-usb:   28 passed
cargo test -p ozma-usb --test functionfs_handoff --test configfs_controller \
  --test functionfs_lifecycle
  # 7 + 12 + 8 passed
cargo check -p ozma-usb -p ozma-node --all-targets     # PASS
rustfmt --edition 2021 --check <touched rust files>    # PASS
```

Live `dummy_hcd` validation (same node invocation as the hardware report):

```text
USB FunctionFS function created and mounted; waiting for the daemon before UDC bind
userspace SCSI media daemon started over FunctionFS polling endpoints and bound after ep0 descriptors
usb-storage 8-1:1.2: USB Mass Storage device detected
scsi 6:0:0:0: Direct-Access OZMA Virtual Media 1.0
[sda] 65536 512-byte logical blocks: (33.6 MB/32.0 MiB)

GET /usb/media after host raw write + sync + read-back:
  state=running commands=62 cbws=62 csws=62 writes=1 reads=49
  data_out=65536 data_in=425082 timeouts=0
  daemon descriptor_writes=1 string_writes=1

SIGTERM:
  userspace SCSI media export stopped; persistence policy applied
  shutdown_seconds=0.228
  gadget removed, UDC empty, FunctionFS unmounted, /dev/sda gone,
  overlay flushed (65,824 bytes), no stray node process
```

The host write used `dd of=/dev/sda bs=4096 seek=256 count=16 conv=fsync`,
`sync`, then `dd if=/dev/sda ... skip=256 count=16`; both files matched
`sha256 6fc179cfd193754e6109ad043f56d146c7e7d7c3623ffceae318266286f58388`.

## 5. Remaining real-UDC/kernel gaps

- No default or CI test mounts FunctionFS, opens a real UDC, or transfers on a
  hardware endpoint. `FunctionFsBulkTransport` is proven only with loopback
  endpoints; a real FunctionFS/UDC test remains env-gated and `#[ignore]`.
- The daemon's real endpoint path uses `PollingFileBulkTransport` and
  `PollingFileEp0Io`; the blocking `FileEndpointIo` remains available for
  callers that supply their own host/UDC timeout. Hardware deadlines are proven
  only against OS pipes/FIFOs; a real UDC transfer remains env-gated.
- The daemon no longer requires an operator-supplied mount in the node path:
  `FunctionFsHandoff` creates the function, mounts FunctionFS, starts the
  daemon, waits for `ep0`, and binds the UDC. This needs root (or the relevant
  mount/ConfigFS privileges); without them mount/bind failures are typed and
  rolled back. The daemon still does not reconcile the `--usb-media` ConfigFS
  kernel LUN with the userspace FunctionFS function.
- `--usb-media-export` auto-starts the userspace SCSI export, but it is separate
  from the `--usb-media` ConfigFS kernel LUN; the node does not reconcile or
  switch between the two paths.
- Hotplug is a bounded poll view, not SSE and not a kernel hotplug monitor. It
  records only route-caused, auto-start, restart/rebind, shutdown, or observed
  transitions.
- Malformed CBWs are typed failures without a CSW; one LUN (LUN 0) and the
  existing SCSI command subset are supported. There is no multi-LUN, class
  control request handling, or `Bulk-Only Mass Storage Reset` route.
- The overlay has no FAT metadata repair or base write-back; writes stay in the
  bounded overlay and persist only through the checksummed overlay file.
