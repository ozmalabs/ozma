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
- `running` is only reported after a real command cycle completed over an
  attached transport. A configured export is `configured`; an unattached
  export uses `IdleBulkTransport` and cannot claim a transfer.

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

Semantics:

- Raw media defaults to read-only. `read_only: false` opts into the bounded
  copy-on-write overlay; the base file is never opened for writing.
- FAT32 and ISO 9660 are always read-only and reject `read_only: false`.
  They are probed with `MassStorageImage::validate_for_attach` before serving.
- `persist_path` must be absolute and distinct from the base. `reload: true`
  loads an existing checksummed overlay with `MassStorageDevice::load_writable_file`.
- Media state is `configured` immediately after start. It becomes `running`
  only after `MassStorageLoop::step()` writes a CSW. A transport failure marks
  it `failed` with a path-free typed `last_error`; stop marks it `stopped`.
- Overlay status is path-free: geometry, allocation, dirty flag, whether a
  persistence file exists, its size, and the flush/stop policy.
- `GET /usb/hotplug?since=<sequence>&limit=<n>` is capped at 64 retained
  events and never synthesises kernel events. Attach/detach/export events are
  recorded only after the corresponding operation succeeds. `unbind` is
  recorded only when a live gadget snapshot transitions from bound to unbound.
- No route auto-starts an export. The default node path never opens a UDC
  endpoint or claims a live host transfer.

## 3. Tests

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

New node tests in `controller_compat::tests` (6 tests):

- Not-configured, configured, running-after-transfer, and stopped states.
- Writable overlay persistence and reload through the routes.
- Bad path/format/read-only requests, duplicate start, and a typed endpoint
  timeout surfaced as `failed` + `last_error`.
- Bounded hotplug log with attach/detach/export events, `since`/`limit`, and an
  observed unbind; no events are fabricated before an action.
- Configured-but-failed gadget returns typed 503s for media and hotplug routes.
- A userspace export starts on a gadget with no `mass_storage.*` kernel LUN; the
  route falls back to the `usb0` instance label instead of requiring ConfigFS
  mass storage.

Verification run:

```text
cargo test -p ozma-usb -p ozma-node --lib          # 451 passed, 3 ignored
cargo test -p ozma-usb --test mass_storage_loop   # 10 passed
cargo check -p ozma-usb -p ozma-node --all-targets
rustfmt --edition 2021 --check <touched rust files>
```

## 4. Remaining real-UDC/kernel gaps

- No default or CI test mounts FunctionFS, opens a real UDC, or transfers on a
  hardware endpoint. `FunctionFsBulkTransport` is proven only with loopback
  endpoints; a real FunctionFS/UDC test remains env-gated and `#[ignore]`.
- Blocking `FileEndpointIo` reads do not themselves enforce the loop deadline.
  The typed timeout contract is implemented and tested with fakes; a live
  endpoint adapter needs a nonblocking/poll-based deadline (or a host/UDC
  timeout) before hardware timeouts are guaranteed.
- There is no persistent daemon process, restart supervision, or automatic
  transport attach after a FunctionFS `ENABLE` event. The node route configures
  an export and leaves attachment to explicit code.
- The node media route does not auto-start from `--usb-media`; the ConfigFS
  kernel LUN path and the userspace SCSI path remain separate.
- Hotplug is a bounded poll view, not SSE and not a kernel hotplug monitor. It
  records only route-caused or observed transitions.
- Malformed CBWs are typed failures without a CSW; one LUN (LUN 0) and the
  existing SCSI command subset are supported. There is no multi-LUN, class
  control request handling, or `Bulk-Only Mass Storage Reset` route.
- The overlay has no FAT metadata repair or base write-back; writes stay in the
  bounded overlay and persist only through the checksummed overlay file.
