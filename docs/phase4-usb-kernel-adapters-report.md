# Phase 4 USB kernel adapters report

Date: 2026-11-25

Scope: `ozma-usb` only (plus this report and the crate README). No commits, no
sudo, no kernel module loading, no real device export in default tests, and no
changes to `ozma-node`, `ozma-softnode`, `ozma-multiseat`, `ozma-mesh`, or the
controller Python.

## Summary

The two remaining kernel seams now have real implementations while the existing
fail-closed defaults stay in place:

1. `LinuxUsbIpHostBackend` implements `UsbIpKernelBackend` over an argv-only
   `CommandRunner` and an injectable sysfs root.
2. `LinuxUsbIpKernelDataEndpoint` implements `UsbIpKernelDataEndpoint` and opens
   a real USB/IP TCP URB session after `OP_REQ_IMPORT`, with devid translation,
   unlink, deadlines, and typed failures.
3. `PollingFileEndpointIo` / `PollingFileBulkTransport` enforce the
   `MassStorageLoop` per-call deadline with `poll(2)` on real pipes/files, while
   the original blocking `FileEndpointIo`/`FileBulkTransport` remain available.

No `unsafe` was added. `#![forbid(unsafe_code)]` remains in `lib.rs`; the
`O_NONBLOCK`/`poll(2)`/pipe layer uses the already-locked `rustix` crate through
its safe wrappers.

## USB/IP host backend

Files: `ozma-usb/src/usbip_kernel.rs`,
`ozma-usb/tests/usbip_kernel.rs`.

### API

- `LinuxUsbIpHostBackend<R = SystemCommandRunner>`
  - `system()`: `usbip` from `PATH`, real `/sys`.
  - `new(runner, program, UsbIpKernelSysfs)` for an injected runner/sysfs tree.
  - `with_authorization(UsbIpAuthorization)`.
  - peer-facing `authorize_host`, `authorize_peer`, `export_for_peer`,
    `attach_authorized`; local-administration `UsbIpKernelBackend`
    `export`/`unexport`/`attach`/`detach`.
  - `ports()`, `list_local()`.
- `UsbIpKernelSysfs`
  - `system()` / `with_root`.
  - `module_path`, `platform_root`, `host_driver_root`.
  - `vhci_module_present`, `host_module_present`, `vhci_attach_path`,
    `vhci_present`, `bound_bus_ids`, `is_bound`, `require_host`,
    `require_vhci`.
- `parse_attach_port` parses `usbip: info: using port NN` and falls back to a
  `usbip port` listing.

### Behavior

| Operation | Command | Precondition | Failure behavior |
|---|---|---|---|
| export | `usbip bind -b <busid>` | `module/usbip_host` + `bus/usb/drivers/usbip-host` | missing module/driver is `usbip-kernel-required`, no command |
| unexport | `usbip unbind -b <busid>` | `usbip_host` module | typed kernel requirement before argv |
| attach | `usbip attach -r <host> -b <busid>` | `module/vhci_hcd` + `vhci_hcd*/attach` | no reported port is `usbip-kernel-required`, never fabricated |
| detach | `usbip detach -p <port>` | vhci attach attribute | typed kernel requirement before argv |
| ports | `usbip port` | vhci attach attribute | command failure stays `command-failed` |

Additional behavior:

- Host, bus id, and vhci port are validated before argv. Bus ids/hosts may not
  begin with `-`; no shell is used and no string interpolation happens.
- A missing `usbip` executable is translated to `usbip-kernel-required` with
  remediation, not a generic spawn error.
- `export` verifies `bus/usb/drivers/usbip-host/<busid>` after a successful
  bind. If the binding is absent, it runs `usbip unbind -b <busid>` as rollback
  and returns `usbip-kernel-required`; no false export is reported.
- vhci ports are zero-based. `Port 00` is preserved as `0` in attachments and
  detach calls. `UsbIpAttachment::validate` and `UsbIpDaemon::detach` use the
  new zero-accepting vhci validator; the legacy `SubprocessUsbController`
  textual detach and TCP port validation keep their existing 1..=65535 rule.
- `attach` prefers the port printed by `usbip attach`; if absent it runs
  `usbip port` and matches host+bus id. If neither reports a port, the call is
  a typed kernel requirement. No port is derived from array position or state.
- Peer-facing methods run the existing constant-time host/token/fingerprint
  allow-list before any sysfs probe or command. Tokens and fingerprints are
  never retained in the backend, placed in argv, or rendered in errors. The
  local-administration trait methods assume the daemon already authorized the
  peer (the existing `UsbIpDaemon` path).
- `available()` is true only when both modules and the vhci attach attribute are
  visible.

### Tests

`tests/usbip_kernel.rs` (15 passing, 1 ignored):

- availability requires both modules and the vhci attach attribute;
- exact bind/unbind argv and sysfs binding verification;
- bind rollback (`bind` then `unbind`) when the binding is absent;
- attach port from command stdout and from `usbip port` (including port zero);
- no fabricated port when neither source reports one;
- `ports` parsing of host/bus id/port/vendor id;
- missing module, missing platform attribute, and missing executable all run no
  command and return `usbip-kernel-required`;
- non-zero command exit stays `command-failed`;
- host/token/fingerprint denial runs no command and redacts secrets; allowed
  pairing reaches the expected argv;
- invalid host/bus id rejected before sysfs/argv;
- ignored real-kernel bind/unbind gated by `OZMA_USBIP_REAL_BUSID`.

## USB/IP TCP data endpoint

Files: `ozma-usb/src/usbip_tcp.rs`, `ozma-usb/tests/usbip_tcp.rs`.

### API

- `LinuxUsbIpKernelDataEndpoint` (factory, `UsbIpKernelDataEndpoint`)
  - `system()` (TCP), `with_connector(Arc<dyn UsbIpUrbConnector>)`,
    `with_config(LinuxUsbIpDataConfig)`, `open_authorized`.
- `LinuxUsbIpDataConfig`: remote port (default 3240), per-request timeout
  (default 5 s, max 60 s), max transfer bytes, optional `UsbIpAuthorization`.
- `UsbIpUrbConnector` / `UsbIpUrbStream`: injectable dialer/duplex stream.
- `TcpUsbIpConnector`: bounded connect + short read/write timeouts.
- `LinuxUsbIpUrbSession`: one reader thread plus a writer mutex, pending
  submit/unlink maps, condvar wakeups, positive stats; implements
  `UsbIpDataEndpoint`.
- `UsbIpUrbSessionStats`: submits, completions, unlinks, unlink replies,
  timeouts, late completions, errors (no payload bytes).

### Behavior

1. Validates host/bus id and `attachment`; enforces configured authorization
   before any connection attempt. `open` presents no credentials, so a
   token/fingerprint-required config fails closed; `open_authorized` accepts
   explicit credentials.
2. Connects to `host:port` through the connector.
3. Writes a real `UsbIpImportRequest` and validates the 8-byte header:
   - bad version -> `usbip-protocol`;
   - wrong reply opcode -> `usbip-protocol`;
   - `ST_NODEV` -> `usbip-unknown-device`;
   - any other non-OK status -> `usbip-protocol` (no endpoint opened);
   - success requires the 312-byte device record and matching bus id.
4. Computes `remote_devid = (busnum << 16) | devnum`.
5. `submit` rewrites the downstream devid to the remote devid, writes
   `USBIP_CMD_SUBMIT`, and waits for the matching `USBIP_RET_SUBMIT`; the
   returned completion restores the downstream devid, direction, and endpoint.
   Overflow, duplicate seqnum, and encode failures are `UsbIpDataError` values.
6. `unlink` allocates a non-colliding seqnum, writes `USBIP_CMD_UNLINK`, waits
   for `USBIP_RET_UNLINK`, and wakes a pending target with `-ECONNRESET` when
   the remote cancels it.
7. A per-request deadline returns `-ETIMEDOUT` with no payload. A closed reader
   fails all waiters with `-ECONNRESET`. Malformed frames, wrong-version
   replies, and client-only commands close the session with a typed error.
8. `Drop` stops the reader, shuts down the socket, fails pending waiters, and
   joins the reader so no detached thread keeps touching the stream.
9. `Debug` reports counters/pending/closed only, never URB payload bytes.

The TCP adapter is a userspace implementation of the imported URB phase. It
does not hand the socket to the kernel `usbip-host` driver and does not create a
local `vhci_hcd` usbfs device. TLS remains external.

### Tests

`tests/usbip_tcp.rs` (8 passing, 1 ignored) with a loopback TCP fake USB/IP
server using the real codec:

- import reply validation, OUT and IN submits, remote devid translation on the
  wire and restoration in completions;
- unlink forwarded with the remote devid and `-ECONNRESET` status;
- silent remote -> typed `-ETIMEDOUT` without invented bytes;
- a silent import handshake is bounded by `UsbIpProtocolError::TimedOut`;
- `ST_NODEV` -> `usbip-unknown-device`, no session;
- unsupported version -> `usbip-protocol`, no session;
- token-required authorization denial before any connector attempt;
- unavailable connector reported without connecting;
- ignored real remote import gated by
  `OZMA_USBIP_REAL_HOST`/`OZMA_USBIP_REAL_BUSID`.

## Nonblocking FunctionFS endpoint

Files: `ozma-usb/src/functionfs_daemon.rs`,
`ozma-usb/src/mass_storage_loop.rs`,
`ozma-usb/tests/functionfs_polling.rs`.

### API

- `PollingFileEndpointIo`
  - `open(endpoint, path, timeout_ms)`, `from_file(...)`;
  - `read_timeout`, `write_timeout`, `flush_timeout`, `timeout_ms`, `path`;
  - implements `FunctionFsEndpointIo` using the configured default deadline.
- `PollingFileEndpointOpener`: opens `epN` files under a `FunctionFsMount` with
  a default deadline.
- `PollingFileBulkTransport`: owns OUT/IN endpoints and implements
  `MassStorageBulkTransport` by forwarding the loop's per-call timeout.
- Constants `DEFAULT_FUNCTIONFS_ENDPOINT_TIMEOUT_MS = 5000`,
  `MAX_FUNCTIONFS_ENDPOINT_TIMEOUT_MS = 60000`.
- The existing blocking `FileEndpointIo`, `FileEndpointOpener`, and
  `FileBulkTransport` are unchanged.

### Behavior

- The descriptor is switched to `O_NONBLOCK` via safe `rustix` `fcntl`.
- Reads/writes wait with `poll(2)` until ready or the deadline. `poll` is
  restarted on `EINTR`; timeout is `MassStorageEndpointTimeout` so
  `MassStorageLoop` counts it as an endpoint timeout.
- Zero-length reads return `Ok(0)` immediately. A closed writer returns `Ok(0)`
  (EOF); the loop treats zero as a transport failure.
- Reads never consume bytes on timeout. Writes retry short writes until the
  deadline; a timeout after a prefix reports the typed timeout and preserves
  the bytes the kernel already accepted (documented for callers).
- Deadlines are validated to 1..=60000 ms; buffers keep the 64 KiB FunctionFS
  per-transfer ceiling. Closing the handle closes the descriptor.

### Tests

`tests/functionfs_polling.rs` (7 passing, 1 ignored) uses real OS
pipes/FIFOs created in temporary directories:

- immediate zero-length read on an empty pipe;
- empty pipe read times out with `mass-storage-endpoint-timeout`;
- data arrives after readiness and is returned;
- closed writer returns EOF without invented bytes;
- partial write keeps the accepted prefix before a timeout;
- invalid zero deadline rejected;
- a `MassStorageLoop` over FIFOs times out on the stalled OUT endpoint
  (`stats.timeout_errors == 1`), then recovers after a real CBW arrives and
  writes a valid CSW;
- ignored real endpoint test gated by `OZMA_FUNCTIONFS_MOUNT`.

## Node adapter and daemon selection

`ozma-node` now selects the real adapters explicitly instead of leaving them to
an embedder:

- `--usb-functionfs-mount PATH` (requires `--usb-gadget`, `--usb-vendor`, and
  `--usb-media-export`) starts a `FunctionFsMediaDaemon` over the explicit
  mount. The daemon uses `PollingFileBulkTransport` and `PollingFileEp0Io`, so
  the blocking file adapter is never selected for this path. `/usb/media`
  reports `transport.kind: "polling-file"` plus attach/detach/enable/disable/
  unbind/restart counters; `running` still requires a completed command cycle.
- `--usbip` starts `UsbIpServer` on `--usbip-host`/`--usbip-port`. The default
  adapter pair is `NoUsbIpKernel` + `NoUsbIpKernelDataEndpoint`, so a successful
  import remains a typed `usbip-kernel-required` failure.
- `--usbip-linux-adapters` injects `LinuxUsbIpHostBackend`
  (`--usbip-program`, `--usbip-sysfs-root`) and
  `LinuxUsbIpKernelDataEndpoint::system()`. `/usb` reports `usbip.adapter`
  (`linux` or `kernel-required`), `state`, active connections, and honest
  session/import/data-session/denied/protocol/byte counters from
  `UsbIpServerStats`. A failed listener start is logged and leaves the typed
  unavailable state rather than inventing a listener.
- Shutdown stops the USB/IP listener (joining its accept loop) before stopping
  the media daemon/export and applying overlay persistence.

No default test starts a real listener, invokes `usbip`, loads a module, or
uses root. The node tests assert the default kernel-required status and that the
FunctionFS mount path selects the polling daemon without claiming `running`.

## Verification

```bash
cargo test -p ozma-usb
cargo check -p ozma-usb --all-targets
rustfmt --edition 2021 --check \
  ozma-usb/src/lib.rs ozma-usb/src/usbip_kernel.rs ozma-usb/src/usbip_tcp.rs \
  ozma-usb/src/usbip.rs ozma-usb/src/usbip_daemon.rs \
  ozma-usb/src/functionfs_daemon.rs ozma-usb/src/mass_storage_loop.rs \
  ozma-usb/src/media_daemon.rs \
  ozma-usb/tests/usbip_kernel.rs ozma-usb/tests/usbip_tcp.rs \
  ozma-usb/tests/functionfs_polling.rs
```

The node-side selection was verified with
`cargo test -p ozma-usb -p ozma-node --lib` and
`cargo check -p ozma-usb -p ozma-node --all-targets`. The default USB/IP status
test asserts the typed kernel requirement, and the FunctionFS media auto-start
test asserts the polling daemon path and no fake `running`.

`cargo test -p ozma-usb` result: 290 passed, 0 failed, 8 ignored. The ignored
tests are the pre-existing live/real-kernel cases plus the new USB/IP and
FunctionFS environment-gated cases.

## Remaining hardware/kernel gaps

- The TCP data adapter implements the wire protocol in userspace, but does not
  hand the connected socket to the kernel `usbip-host` driver, does not open a
  local `vhci_hcd` usbfs device, and does not execute USBDEVFS URBs. A node that
  needs the kernel vhci device still uses the existing `usbip attach` CLI path
  through `LinuxUsbIpHostBackend`.
- No in-tree kernel module, `usbip-host` stub, usbfs ioctl layer, or kernel
  hotplug monitor. The host backend observes sysfs only when an operation runs.
- Real-kernel bind/unbind and real remote import are `#[ignore]` and
  environment-gated; default CI ran them against fake sysfs/scripted runners and
  a loopback TCP server only.
- ISO descriptors are validated and forwarded, but ISO wire-padding compaction
  and packet-level scheduling remain endpoint duties; the TCP session forwards
  the codec's descriptor arrays as-is.
- The data session has no TLS. The USB/IP protocol has no in-band TLS and this
  crate still does not terminate it; an external terminator/tunnel is required.
- The node selects the polling transport and Linux USB/IP adapters only when
  the explicit flags are set. It does not auto-detect a mounted FunctionFS
  directory, mount/UDC-bind on its own, export local devices into USB/IP, or
  provide an in-process TLS terminator.
- No real UDC/FunctionFS data transfer in the default suite; the polling
  transport is proven against OS pipes/FIFOs, and the real endpoint test remains
  opt-in.
