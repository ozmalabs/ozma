# Phase 4 USB/IP Data Relay Report

Status: implementation complete in `ozma-usb`. The control server now hands the
same authenticated connection to the URB relay after a successful authorized
import, through an injectable kernel data-endpoint seam. The default seam is a
typed `usbip-kernel-required` failure, so no data path is fabricated when a
kernel `vhci`/`usbip-host` adapter is absent. The codec, relay, and server
handoff are exercised entirely with in-memory transports and endpoints. No
kernel modules, `vhci_hcd`/`usbip-host` devices, `usbip` invocations, sudo, real
device captures, or persistent sockets are used by the default test suite.

## Scope

This phase added the post-IMPORT USB/IP data path without changing the existing
control plane or authorization:

- `ozma-usb/src/usbip_urb.rs` — typed 48-byte URB codec for
  `USBIP_CMD_SUBMIT`, `USBIP_RET_SUBMIT`, `USBIP_CMD_UNLINK`, and
  `USBIP_RET_UNLINK`, including bounded ISO packet descriptors and
  payload-redacted `Debug`.
- `ozma-usb/src/usbip_relay.rs` — deterministic SUBMIT/RET pairing core,
  injectable `UsbIpDataEndpoint`, the `UsbIpKernelDataEndpoint` adapter seam
  with the explicit `NoUsbIpKernelDataEndpoint` default, bounded stream session
  with concurrent in-flight URBs, unlink cancellation, idle timeout, and
  disconnect cleanup.
- `ozma-usb/src/usbip_server.rs` — post-import handoff on the same authenticated
  transport, injectable data-endpoint factory/relay bounds, success only after
  both kernel attach and data-endpoint open, and rollback on data-seam failure.
- `ozma-usb/src/error.rs` — typed `usbip-urb` and `usbip-relay` error kinds.
- `ozma-usb/tests/usbip_urb.rs`, `ozma-usb/tests/usbip_relay.rs`, and extended
  `ozma-usb/tests/usbip_server.rs`.
- `ozma-usb/README.md` and this report.

Attach/import still requires a real kernel backend and a real data adapter. The
default `NoUsbIpKernel`/`NoUsbIpKernelDataEndpoint` pair continues to return
`usbip-kernel-required`; the control server never creates a successful import
without both, and the data relay never fabricates a bus id, port, or payload.

## URB codec coverage

Every message starts with the packed 20-byte `usbip_header_basic` followed by a
28-byte command union:

| Offset | Length | Field |
|---|---|---|
| 0 | 4 | `command` |
| 4 | 4 | `seqnum` |
| 8 | 4 | `devid` |
| 12 | 4 | `direction` (0 = OUT, 1 = IN) |
| 16 | 4 | `ep` (0..=15) |
| 20 | 4 | submit flags / completion status / unlink seqnum or status |
| 24 | 4 | transfer length / actual length |
| 28 | 4 | start frame |
| 32 | 4 | number of packets |
| 36 | 4 | interval / error count |
| 40 | 8 | setup bytes / completion padding |
| 48 | n | payload (`OUT` submit or `IN` completion) |
| 48+n | 16×m | ISO packet descriptors (`offset`, `length`, `actual_length`, `status`) |

All integers are big-endian. The typed surface is:

| Type | Purpose |
|---|---|
| `UsbIpCmdSubmit` | OUT payload, IN request length, flags, setup, ISO descriptors |
| `UsbIpRetSubmit` | status, actual length, IN payload, ISO descriptors |
| `UsbIpCmdUnlink` | fixed 48-byte request with target `seqnum` |
| `UsbIpRetUnlink` | fixed 48-byte reply with status |
| `UsbIpUrb` / `UsbIpUrbCommand` | unified decode/encode and command identity |
| `UsbIpDirection` / `UsbIpIsoPacketDescriptor` | direction and ISO records |

A non-ISO transfer uses the kernel `number_of_packets = 0xffffffff` sentinel
(`0` is also accepted for compatibility). An IN submit carries no payload; an
OUT completion carries no payload even when `actual_length` reports consumed
bytes. This matches the kernel wire format and the tests assert it explicitly.

### Bounds and typed errors

- Transfer/actual lengths: `USBIP_URB_MAX_TRANSFER_BYTES`, which reuses
  `USBIP_MAX_FRAME_BYTES` (112652 bytes). A header is checked before the body is
  allocated.
- ISO descriptors: `USBIP_MAX_ISO_PACKETS = 128`; `number_of_packets` must match
  the descriptor array, `offset + length` must fit the declared buffer (submit)
  or the transport ceiling (completion), and `actual_length <= length`.
- Endpoint numbers above 15 are rejected.
- Payload direction is enforced: OUT submit data length must equal
  `transfer_buffer_length`; IN submit data must be empty; IN completion data
  length must equal `actual_length`; OUT completion data must be empty.
- `UsbIpUrbError` distinguishes `Truncated`, `TrailingBytes`, `UnknownCommand`,
  `UnsupportedDirection`, `InvalidEndpoint`, `InvalidField`, `Oversized`,
  `CountMismatch`, `IsoPacketOutOfBounds`, and `InconsistentData`. It maps to
  `UsbError::UsbIpUrb` with kind `usbip-urb`.
- Kernel-style server replies conventionally encode `direction = 0` even for IN
  data. `UsbIpRetSubmit::decode_with_direction` accepts the original request
  direction explicitly; the relay encoder preserves the actual direction so its
  own frames remain self-delimiting.
- `Debug` for submit/completion types prints `data_len` and ISO descriptor count,
  never payload bytes.

Golden tests assert exact byte offsets and values for all four commands, not
only round trips. Truncation, trailing bytes, unknown commands, bad direction,
bad endpoint, negative signed lengths, oversize transfer/packet counts, ISO
out-of-bounds descriptors, count mismatches, and bounded decode are distinct
cases.

## Relay behavior

`UsbIpDataEndpoint` is the injectable device boundary:

```rust
pub trait UsbIpDataEndpoint: Send + Sync {
    fn submit(&self, request: &UsbIpCmdSubmit)
        -> Result<UsbIpRetSubmit, UsbIpDataError>;
    fn unlink(&self, devid: u32, seqnum: u32)
        -> Result<UsbIpRetUnlink, UsbIpDataError>;
}
```

- `UsbIpRelay` tracks only metadata (`seqnum`, `devid`, direction, endpoint,
  declared lengths, start time) while a URB is in flight.
- `register_submit` validates the submit, rejects duplicate seqnums, and
  enforces `max_in_flight` (`1..=256`, default 32).
- `complete_submit` pairs by seqnum, verifies devid/direction/actual length and
  codec consistency, and returns a late-completion signal when the URB was
  already unlinked.
- `unlink` removes a target by its `unlink_seqnum`; a `devid` mismatch fails
  without losing the target. A failed endpoint cancellation restores the target
  so a later completion can still pair. A successful unlink drops that submit;
  no `RET_SUBMIT` follows.
- `drain_in_flight` returns every outstanding URB for `endpoint.unlink` during
  disconnect/idle/error cleanup.

`serve_data_session` reads bounded frames from any `UsbIpTransport`:

1. It parses the 48-byte header, computes the exact bounded frame length, then
   reads the body.
2. Client `CMD_SUBMIT` frames are registered and dispatched to one bounded
   worker thread each, so completions may be written out of order.
3. Excess submits receive `RET_SUBMIT` with `-EBUSY`; duplicate seqnums receive
   `-EINVAL`; oversized transfers/ISO counts receive `-EMSGSIZE`. The session
   stays alive.
4. Endpoint `Err` values become negative-status completions with zero bytes;
   malformed endpoint completions become `-EPROTO`. No device data is invented.
5. `CMD_UNLINK` removes the target, calls `endpoint.unlink`, and writes
   `RET_UNLINK` (`-ECONNRESET` for a successful cancellation, `0` when the
   submit already completed). If the endpoint cannot cancel, the target is
   restored.
6. Peer close, stop, idle deadline, and protocol errors drain and unlink all
   in-flight URBs before the outcome is returned.

`UsbIpRelayConfig` also bounds transfer bytes, ISO descriptors, and an optional
idle deadline (default 30 s; `None` disables it). Relay outcomes record counts
and byte totals, never payload contents.

## Server wiring and kernel data seam

`serve_session_with_data` (used by `UsbIpServer` and exported for embedders)
extends the existing control session without changing control semantics:

1. It calls `UsbIpDaemon::accept_connection` before reading a frame, so the
   daemon connection ceiling and host/token/key-fingerprint authorization apply
   before both the control and data phases.
2. `OP_REQ_DEVLIST` is answered by the existing control loop and returns to the
   next frame. It never opens a data endpoint and never enters the relay.
3. A failed `OP_REQ_IMPORT` (unknown/descriptor-less bus id, authorization
   denial, kernel attach failure, or a backend with no `vhci` port) writes the
   existing error status with no device body, rolls back any partial attach, and
   returns. It never opens a data endpoint and never enters the relay.
4. A successful attach is followed by exactly one
   `UsbIpKernelDataEndpoint::open(&attachment)` call, before the success reply.
   The seam distinguishes an absent adapter (`UsbError::UsbIpKernelRequired`,
   no success body, local attach rolled back on a best-effort basis) from an
   opened endpoint whose later runtime failures become negative-status
   `USBIP_RET_SUBMIT` frames with zero bytes.
5. Only after both succeed does the server write the 320-byte
   `OP_REP_IMPORT`, record `attached_bus_id`, and call `serve_data_session` on
   the same transport. `UsbIpSessionOutcome::data` embeds the relay counters.
6. The tracked daemon connection is closed on every exit path, including after
   the data session ends or errors.

The seam/factory is bounded by construction: one endpoint per successful import,
owned by one session, and sessions are already bounded by
`UsbIpServerConfig::max_connections`. `UsbIpServer::with_data_endpoint` and
`with_relay_config` inject the adapter and relay bounds; the convenience
`serve_session` uses `NoUsbIpKernelDataEndpoint` and default relay bounds.

## Tests

New suites:

- `tests/usbip_urb.rs` (14 tests): layout constants; exact OUT submit and IN
  completion offsets; fixed unlink frames and zero padding; OUT/IN payload
  rules; kernel `number_of_packets` sentinel; kernel-style direction-zero
  completion decoding with explicit context; unified round trips; truncation vs
  trailing bytes; unknown command/direction/endpoint; negative/oversized
  lengths; ISO round trips and offset/length/count bounds; bounded decode;
  payload redaction and `usbip-urb` error kind.
- `tests/usbip_relay.rs` (9 tests): one and many in-flight URBs; out-of-order
  completions; unlink before completion (late completion dropped) and after
  completion (not-found reply); endpoint error status with no payload; disconnect
  cleanup; in-flight ceiling `-EBUSY`; idle timeout cleanup; outcome `Debug`
  without payload.
- `src/usbip_relay.rs` unit tests (4): deterministic core pairing/ceiling,
  duplicate/late completion handling, devid mismatch without losing the target,
  and the no-op kernel seam returning a typed `usbip-kernel-required` error
  without opening an endpoint.
- `tests/usbip_server.rs` (23 tests, extended): the original control-plane
  coverage plus a control-only DEVLIST that never opens an endpoint; a failed
  import that never enters the data phase; successful import relaying one and
  many URBs on the same connection; unlink cancellation; disconnect cleanup;
  an endpoint error returned as a negative-status RET with zero bytes; the
  default no-kernel seam typed with no success body and an attach rollback;
  authorization enforced before both phases; and a loopback TCP control + URB
  exchange through the same codec.

Commands run:

```bash
cargo test -p ozma-usb
cargo check -p ozma-usb --all-targets
rustfmt --edition 2021 --check \
  ozma-usb/src/usbip_server.rs ozma-usb/src/usbip_relay.rs \
  ozma-usb/src/lib.rs ozma-usb/src/error.rs \
  ozma-usb/tests/usbip_server.rs ozma-usb/tests/usbip_relay.rs \
  ozma-usb/tests/usbip_urb.rs
```

## Remaining kernel/dataplane gaps

- There is still no in-tree kernel `usbip-host` stub or `vhci_hcd` adapter. A
  production `UsbIpKernelDataEndpoint` would open these URBs against a real
  kernel USB device (or a userspace device backend) and must supply genuine
  completion bytes/statuses. The default seam fails closed with
  `usbip-kernel-required`.
- The control server now performs the post-import handoff, but a node-level
  orchestration/route that injects the kernel adapter, selects relay bounds,
  and exposes data-session counters is not implemented. Client-side import
  orchestration remains the existing argv-only controller/daemon path.
- Attach/import success still requires `UsbIpKernelBackend::attach` to return a
  real local port **and** the data seam to open a real endpoint; default
  deployments remain `usbip-kernel-required`.
- ISO descriptors are validated and round-tripped, but wire-padding compaction
  into a real URB buffer and packet-level scheduling are endpoint duties.
- TLS remains external; the listener and relay are plaintext.
- No live device capture, kernel module loading, sudo, or hardware data-path
  evidence was produced. `ozma-node`, `ozma-multiseat`, and `ozma-audio` were
  not modified.
