# Phase 4 USB/IP Control Wire Protocol Report

Status: implementation complete in `ozma-usb` (control plane only). No kernel
modules, devices, `usbip` invocations, or persistent sockets are used by the
default test suite.

## Scope

This phase added the real USB/IP control wire protocol and a bounded server to
`ozma-usb`:

- `ozma-usb/src/usbip_protocol.rs` — typed codec for protocol version `0x0111`.
- `ozma-usb/src/usbip_server.rs` — injectable stream transport, session loop,
  bounded TCP listener, connection/frame/idle limits.
- `ozma-usb/src/usbip_daemon.rs` — export descriptors, `device_list`, and
  descriptor-aware export methods; existing authorization and kernel boundary
  reused unchanged.
- `ozma-usb/src/error.rs` — `usbip-protocol` and `usbip-unknown-device` typed
  error kinds.
- `ozma-usb/tests/usbip_protocol.rs` and `ozma-usb/tests/usbip_server.rs`.

USB/IP bulk data transfer (URB submit/retrieve), a server-side `usbip-host`
stub, kernel module lifecycle, and in-process TLS remain out of scope.

## Protocol coverage

The codec implements every control message in the version-1.1.1 handshake:

| Opcode | Constant | Direction | Encoded body |
|---|---|---|---|
| `0x8005` | `OP_REQ_DEVLIST` | client → server | none (8-byte header) |
| `0x0005` | `OP_REP_DEVLIST` | server → client | `ndev: u32` + device records |
| `0x8003` | `OP_REQ_IMPORT` | client → server | 32-byte NUL-padded `busid` |
| `0x0003` | `OP_REP_IMPORT` | server → client | 312-byte device record on success |

Every frame starts with the 8-byte packed `op_common` header (`version: u16`,
`code: u16`, `status: u32`) in network byte order. Status values `ST_OK`,
`ST_NA`, `ST_DEV_BUSY`, `ST_DEV_ERR`, `ST_NODEV`, and `ST_ERROR` are modelled;
unknown values are rejected.

Device records match the kernel's packed `usbip_usb_device`:

- `path[256]`, `busid[32]` NUL-padded printable ASCII;
- `busnum`, `devnum`, `speed` (`u32`);
- `idVendor`, `idProduct`, `bcdDevice` (`u16`);
- `bDeviceClass`, `bDeviceSubClass`, `bDeviceProtocol`,
  `bConfigurationValue`, `bNumConfigurations`, `bNumInterfaces` (`u8`);
- followed in `OP_REP_DEVLIST` only by `bNumInterfaces` 4-byte
  `usbip_usb_interface` records (class/subclass/protocol plus zero padding).

The codec exposes `UsbIpDevListRequest`, `UsbIpDevListReply`,
`UsbIpImportRequest`, and `UsbIpImportReply` with strict encode/decode. The
`OP_REP_IMPORT` success body is exactly 320 bytes (8-byte header + 312-byte
device record); error replies are exactly 8 bytes with no device body.

### Bounds and typed errors

- Device count: `USBIP_MAX_DEVICES = 256`.
- Interface count: `USBIP_MAX_INTERFACES = 32`.
- Frame ceiling: `USBIP_MAX_FRAME_BYTES = 112652`
  (`12 + 256 * (312 + 32 * 4)`).
- Strings must be non-empty printable ASCII and fit their buffer with a
  terminating NUL; NUL padding is enforced on encode.
- `UsbIpProtocolError` distinguishes `Truncated`, `TrailingBytes`,
  `UnsupportedVersion`, `UnknownOpcode`, `UnexpectedOpcode`, `UnknownStatus`,
  `Oversized`, `InvalidString`, `CountMismatch`, and `InconsistentStatus`.
  It converts to `UsbError::UsbIpProtocol` with kind `usbip-protocol`.
- Only version `0x0111` is accepted. The server answers a mismatched version
  with an error reply carrying its own `0x0111` header, then closes; it never
  pretends to negotiate an unsupported version.

The codec is tested against fixed byte offsets and header vectors, not only
round trips, so endianness and field order regressions are visible.

## Authorization and attach behavior

The server reuses the daemon's existing enforcement rather than a parallel
check:

1. On accept, `serve_session` derives the peer host from
   `UsbIpTransport::peer_host` (an IP string for TCP; a label for in-memory
   transports), obtains out-of-band credentials from the injected
   `UsbIpCredentialSource`, and calls `UsbIpDaemon::accept_connection`.
   That enforces the daemon connection ceiling and the exact
   host/token/key-fingerprint allow-list before a single frame is read.
2. `OP_REQ_DEVLIST` returns only exports that were registered with a
   `UsbIpDeviceDescriptor`. Exports made with the descriptor-less `export`
   method are deliberately omitted: no path, bus number, device number, speed,
   or interface list is invented.
3. `OP_REQ_IMPORT(busid)` must match an advertised descriptor and then calls
   `UsbIpDaemon::attach(host, busid, token, fingerprint)`. That repeats the
   same authorization and reaches the injected `UsbIpKernelBackend`.
4. A success reply (bus id plus 312-byte device record) is written only when
   the backend returned a real local `vhci` port. With `NoUsbIpKernel` or any
   failing backend, the server writes an 8-byte `ST_DEV_ERR` reply with no
   device body and returns `usbip-kernel-required` to the Rust caller.
5. Unknown or descriptor-less bus ids return `usbip-unknown-device` and a
   `ST_NODEV` reply with no device body.
6. Successful export/import transitions append to the existing bounded
   `UsbIpHotplugLog`; tests assert the `export` + `attach` event sequence and
   the assigned port.
7. Accepted connection records are closed on every exit path (peer close,
   stop, timeout, protocol error), and the tests assert the connection list is
   empty afterwards.

The USB/IP wire protocol has no token or fingerprint field. Credentials are
therefore supplied by an embedder hook (for example a TLS terminator's verified
client certificate fingerprint or a trusted proxy token). They are held only
for the session, have a redacted `Debug`, are not serializable, and never
appear in replies, status, errors, or logs. The default source presents
nothing, so token/fingerprint requirements fail closed.

### Limits

`UsbIpServerConfig` bounds:

- simultaneous sessions (`1..=64`, default 8);
- accepted/produced control frames (`12..=112652`, default maximum);
- idle time between complete frames (default 30 s; `None` disables).

Blocked reads use a 50 ms poll interval and check both the stop flag and the
idle deadline, so `UsbIpServerHandle::stop` joins sessions promptly. TLS is not
terminated in-process; `UsbIpTlsPolicy::Required` still fails closed and the
documented deployment is behind an external `usbipd`/TLS terminator or
encrypted tunnel.

## Tests

New suites:

- `tests/usbip_protocol.rs` (12 tests): layout constants, opcode/status golden
  bytes, field-offset network byte order, DEVLIST empty/populated round trips,
  fixed 40-byte IMPORT request, 320-byte IMPORT success body, error-header-only
  reply, bad version/opcode/status, request-status inconsistency, truncated vs
  trailing frames, oversized `ndev`/interfaces/frame/strings, non-printable
  strings, inconsistent bodies, and `UsbError` conversion.
- `tests/usbip_server.rs` (15 tests): in-memory duplex empty/populated DEVLIST,
  descriptor-only advertising, authorized import with a fake kernel port,
  denied host/token/fingerprint cases never reaching the kernel, redacted
  credentials, kernel-required and unknown-device replies with no device body,
  bad version error reply, unknown opcode/truncated frames, configured frame
  ceiling, idle timeout, concurrent session ceiling, stopped daemon, config
  bounds, and a real loopback TCP (`127.0.0.1:0`) DEVLIST+IMPORT exchange using
  the same codec with a fake kernel.

Verification run:

```text
cargo test -p ozma-usb
  -> 218 passed; 0 failed; 5 ignored (ozma-usb)
cargo check -p ozma-usb --all-targets
  -> ok
rustfmt --edition 2021 --check <touched files>
  -> ok
```

No test loads a kernel module, runs `usbip`, exports a real device, or opens a
USB device node. The loopback test binds an ephemeral loopback TCP socket only.
TLS tests remain the existing fail-closed ones in `tests/usbip_security.rs` and
`tests/usbip_daemon.rs`.

## Remaining gaps

### Kernel / data path

- No USB/IP bulk data path: the server answers control requests only and never
  serves URBs over the second USB/IP socket.
- No server-side `usbip-host`/`vhci_hcd` adapter is implemented. A real
  deployment must inject a `UsbIpKernelBackend` that drives the kernel with the
  required privileges; the default backend, and all test fakes, explicitly
  return `usbip-kernel-required` rather than fabricating a successful attach.
- `OP_REQ_IMPORT` currently reflects a successful local attach through that
  backend. A full `usbipd` would separate server-side export preparation from
  client-side `vhci` attachment; that split (and detach-on-disconnect) is not
  implemented.
- No kernel hotplug monitor; the existing hotplug log records library-level
  export/attach/detach transitions only.
- No real-device or kernel-module integration test (by design: no sudo,
  modules, or device export in this phase).

### TLS / transport

- The control listener is plaintext. No rustls/TLS handshake is implemented or
  claimed. `UsbIpTlsPolicy::Required` fails closed.
- Deployment requires an external TLS terminator/tunnel (for example
  WireGuard/stunnel) or a trusted network boundary; the crate cannot protect
  the port itself.
- Peer credentials for the control session are out-of-band by design because
  the USB/IP wire format has no token/fingerprint fields. The embedder must
  wire the terminator/proxy to a `UsbIpCredentialSource`.

### Integration

- No `ozma-node` route or capability advertisement for the server was added in
  this phase (scope was `ozma-usb` only).
- Client-side import orchestration (`usbip attach`/port tracking) remains the
  existing argv-only controller/daemon path.
