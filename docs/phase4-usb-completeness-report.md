# Phase 4 USB completeness report

Status: code complete for the library gaps in `ozma-usb`; no privileged or
hardware validation was performed in this pass. The tree was committed and
green before this work, and this change is intentionally **not committed**.

Scope:

- `ozma-usb/src/functionfs_lifecycle.rs`, `functionfs_daemon.rs`,
  `functionfs.rs`, `lib.rs`, `error.rs`
- new `ozma-usb/src/mass_storage.rs`
- new `ozma-usb/src/usbip_daemon.rs`
- three new integration suites under `ozma-usb/tests/`
- `ozma-usb/README.md`
- this report

`docs/parity-matrix.md` and `docs/plan-execution-status.md` were not edited.
`ozma-node` and `ozma-audio` were not edited.

## 1. FunctionFS lifecycle

### Typed state machine

`FunctionFsStateMachine` now exposes an event-driven state enum:

```text
Unconfigured
  -> DescriptorsBound   (ep0 descriptor blob + string table accepted)
  -> Ep0Ready           (daemon can service the ep0 event stream)
  -> Enabled            (FunctionFS ENABLE event; endpoints open)
  -> Running            (at least one endpoint transfer completed)
  -> Disabled           (FunctionFS DISABLE event; endpoints closed)
  -> Unbound            (UNBIND event or cleared UDC)
```

`FunctionFsDeviceStatus` carries `state`, `descriptors_bound`, `ep0_ready`,
`udc_bound`, `enabled`, `running`, and `endpoint_events`. Explicit predicates
`udc_bind_ready()` and `udc_unbind_ready()` gate UDC writes/clears. Every
out-of-order transition (ep0 before descriptors, enable before ep0, endpoint
traffic while disabled, duplicate UDC binds, unbind before descriptors) returns
`UsbError::InvalidFunctionFsState` (`invalid-functionfs-state`).

### Daemon-core integration

`FunctionFsDaemonCore` embeds the machine:

- a successful `write_descriptors` moves it through `DescriptorsBound` to
  `Ep0Ready` (the core owns an `ep0` transport, so a successful write proves
  the event stream works);
- `Bind` records `udc_bound`; `Unbind` clears it;
- `Enable`/`Disable` move between `Enabled`/`Disabled`;
- only a successful endpoint read/write with non-zero bytes calls
  `record_endpoint_event`, which is the sole path to `Running`;
- `shutdown` closes endpoints and leaves the machine at `Disabled` (never
  `Running`).

`FunctionFsDaemonStatus` now includes `state`, `running`, `udc_bound`, and
`endpoint_events`. The existing `live` flag keeps its previous meaning
(descriptors + strings + enable), while `running` is the stricter claim and is
never set by descriptors, a mount, a readiness marker, a UDC bind, or an enable
event.

### ConfigFS lifecycle helper

`FunctionFsLifecycle` uses the machine for:

- `write_descriptors` → `DescriptorsBound`;
- `wait_for_ready` after the daemon marker → `Ep0Ready`;
- `bind` requires `udc_bind_ready()` and then records the UDC bind;
- `unbind`/`cleanup` clear the UDC and reset the machine;
- `FunctionFsLifecycleStatus.device` plus `running()`, `udc_bind_ready()`, and
  `udc_unbind_ready()` expose readiness without claiming endpoint traffic.

The helper never opens `epN`, so its `running` status is always false. The
ignored `real_udc_binds_after_readiness_and_unbinds` test asserts that even a
real UDC bind plus readiness marker does not claim `running`.

### Tests

`ozma-usb/tests/functionfs_state.rs`:

- ordered transitions and every readiness predicate;
- out-of-order transitions are typed errors;
- daemon status is `Ep0Ready` then `Enabled` but not `Running` until a real
  endpoint write; disable/unbind clear it;
- `Bind` before ep0 is rejected; bind/unbind tracking;
- lifecycle helper refuses `bind` before the readiness marker and never reports
  `running`;
- ignored, env-gated real UDC test
  (`OZMA_FUNCTIONFS_UDC`, `OZMA_FUNCTIONFS_MOUNT`,
  `OZMA_FUNCTIONFS_DAEMON`, `OZMA_FUNCTIONFS_READY_MARKER`).

The existing `functionfs_lifecycle` and `functionfs_daemon` suites still pass
unchanged in behavior.

## 2. Overlay in the SCSI/media data path

New module `ozma-usb/src/mass_storage.rs`.

`MassStorageDevice` implements the block-data portion of USB Mass Storage
Bulk-Only Transport:

- CBW parsing (`ScsiCommandBlock`) with signature/CDB-length validation;
- `TEST UNIT READY`, `REQUEST SENSE`, `INQUIRY`, `MODE SENSE(6/10)`,
  `START STOP UNIT`, `PREVENT ALLOW MEDIUM REMOVAL`, `READ CAPACITY(10)`,
  `READ FORMAT CAPACITIES`, `READ(10)`, `WRITE(10)`, `SYNCHRONIZE CACHE(10)`;
- CSW status/residue and fixed-format sense data (`ScsiReply`/`ScsiSense`);
- a 1 MiB (2048-sector) per-command transfer bound.

Backing:

- `read_only_bytes`, `read_only_file`, and `read_only_fat32` serve raw, ISO, or
  FAT32 media directly. The FAT32/ISO builders were not changed. Writes are
  rejected with `read-only-media` or a SCSI `DATA PROTECT / WRITE PROTECTED`
  sense.
- `writable_bytes`, `writable_file`, and `load_writable_file` wrap the same
  bounded `WriteOverlay` used by NBD. Reads prefer overlay blocks and fall
  back to the immutable base; writes copy the touched base block and patch the
  overlay. The base file is opened read-only.
- `SYNCHRONIZE CACHE(10)` calls `flush`, which commits the checksummed
  persistence file when `MassStoragePersistPolicy::persist_on_flush` is set.
  `stop()` commits when `persist_on_stop` is set. `commit()` is the explicit
  manual-policy path. `load_writable_file` reloads a validated persistence
  file.
- Out-of-range ranges, oversized transfers, and allocation-ceiling failures are
  typed (`overlay-bounds` at the API, SCSI `ILLEGAL REQUEST`/`MEDIUM ERROR` at
  the wire). Failed writes do not partially allocate.

### Tests

`ozma-usb/tests/mass_storage_scsi.rs`:

- SCSI write then read-back through the overlay;
- base bytes remain unchanged (caller vector and base file);
- FLUSH / stop persistence and reload through a new device;
- manual policy does not persist implicitly but explicit commit does;
- read-only raw media rejects direct and SCSI writes (`read-only-media`,
  `DATA PROTECT`, WP mode-sense bit);
- typed bounds, LBA-out-of-range sense, phase errors, malformed CBW, unknown
  opcode (`INVALID COMMAND`);
- overlay exhaustion does not allocate a partial block and reports
  `MEDIUM ERROR / WRITE ERROR`;
- a generated read-only FAT32 image is served byte-identically.

## 3. USB/IP daemon

New module `ozma-usb/src/usbip_daemon.rs`.

`UsbIpDaemon` is a bounded in-process control-plane state machine:

- `UsbIpDaemonState::{Stopped, Listening}` with idempotent `start`/`stop`;
- `export`, `export_with_token`, `list`, `unexport`, `attach`,
  `attach_with_token`, `detach`, `accept_connection`, `close_connection`;
- `UsbIpDaemonConfig` with host/port, authorization, TLS policy,
  `max_connections` (1–64), and `hotplug_capacity` (1–64);
- bounded `UsbIpConnection`s holding only `UsbIpPeerGrant` metadata;
- the existing bounded `UsbIpHotplugLog` records
  export/unexport/attach/detach with monotonic sequences.

Authorization reuses `UsbIpAuthorization::authorize_peer`; a configured host
allow-list, token, and key fingerprint are checked before any kernel call.
Denials never reach the kernel backend, never change export/attachment state,
and never include the supplied token or fingerprint. Status/Debug/JSON contain
no raw secrets.

The kernel boundary is an injectable `UsbIpKernelBackend`. `NoUsbIpKernel` is
the explicit default and returns typed
`UsbError::UsbIpKernelRequired` (`usbip-kernel-required`) for export, unexport,
attach, and detach, naming `vhci_hcd`/`usbip-host`. A failed detach keeps the
recorded attachment. The daemon never claims a live USB/IP attachment without a
backend result.

TLS policy is unchanged and fail-closed: `UsbIpTlsPolicy::Required` is rejected
during validation with `usbip-tls-unsupported`; `UsbIpDaemonStatus.tls` always
reports `available: false`. The documented deployment requires an external
`usbipd` behind a TLS terminator or encrypted tunnel. No in-process TLS is
claimed or faked.

### Tests

`ozma-usb/tests/usbip_daemon.rs`:

- authorized attach reaches the fake kernel; host/token/fingerprint-denied
  attaches do not and leave state/events unchanged;
- no token/fingerprint in errors, status JSON, or `Debug`;
- bounded peer connections, connection-limit error, stop clears sessions, and
  operations on a stopped daemon are typed;
- export/attach/detach/unexport cleanup, kernel calls, and a bounded
  drop-oldest hotplug log with monotonic sequences;
- default backend gives `usbip-kernel-required` for export/attach/unexport and
  for detach after an attach-only backend; no fabricated state;
- TLS-required fails closed and disabled policy still reports TLS unavailable;
- invalid port/connection/event bounds are rejected.

## Verification

Run in this pass:

```bash
cargo test -p ozma-usb
cargo check -p ozma-usb --all-targets
rustfmt --edition 2021 --check <touched .rs files>
```

Results:

- `cargo test -p ozma-usb`: **181 passed, 0 failed, 5 ignored**. Ignored tests
  are the pre-existing real-ConfigFS, real FunctionFS mount, opt-in NBD
  boot/QMP checks, and the new ignored real-UDC lifecycle test.
- `cargo check -p ozma-usb --all-targets`: clean.
- `rustfmt --edition 2021 --check` on every touched Rust file: clean.

No test mounted a filesystem, opened a block device, loaded a kernel module,
bound a real UDC, ran a real `usbip` command, or required root.

## Remaining kernel/hardware gaps

- **FunctionFS real UDC/mount**: the ignored test is the only path that writes
  a real ConfigFS gadget/UDC; it requires a mounted FunctionFS root, a free UDC,
  and an externally started daemon. There is no persistent daemon supervisor,
  restart handling, real `ep0` event loop in the node, or vendor protocol
  handler. Suspend/resume events are decoded but not acted on.
- **SCSI transport wiring**: `MassStorageDevice` is a complete in-memory BBB
  data-path model, but it is not yet driven by the FunctionFS bulk endpoints or
  exposed through a node route. No real host enumeration or SCSI transport was
  exercised.
- **USB/IP kernel data path**: no `vhci_hcd`, `usbip-host`, socket listener, or
  USB/IP wire protocol. Export/attach/detach require an injected backend; the
  default is a typed unsupported error. No in-process TLS; an external
  `usbipd`/TLS terminator is mandatory for encrypted deployments. No node route
  exposes the daemon yet.
- **Overlay durability**: no FAT metadata/consistency repair, directory-entry
  coalescing, compaction, or write-back to the base image. The base is always
  immutable, and generated images are never written to disk/block devices.
- **Hardware validation**: no `dummy_hcd`, hardware UDC, real USB/IP attach, or
  live mass-storage enumeration was run in this pass.
