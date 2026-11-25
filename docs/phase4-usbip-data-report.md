# Phase 4 USB/IP Data Relay Report

Status: implementation complete in `ozma-usb` at the library level. The codec
and relay are exercised entirely with in-memory transports and endpoints. No
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
  injectable `UsbIpDataEndpoint`, bounded stream session with concurrent
  in-flight URBs, unlink cancellation, idle timeout, and disconnect cleanup.
- `ozma-usb/src/error.rs` — typed `usbip-urb` and `usbip-relay` error kinds.
- `ozma-usb/tests/usbip_urb.rs` and `ozma-usb/tests/usbip_relay.rs`.
- `ozma-usb/README.md` and this report.

Attach/import still requires a real kernel backend. The default
`NoUsbIpKernel` continues to return `usbip-kernel-required`; the data relay never
creates a successful import and never fabricates a bus id, port, or payload.

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
- `src/usbip_relay.rs` unit tests (3): deterministic core pairing/ceiling,
  duplicate/late completion handling, and devid mismatch without losing the
  target.

Commands run:

```bash
cargo test -p ozma-usb
cargo check -p ozma-usb --all-targets
rustfmt --edition 2021 --check \
  ozma-usb/src/usbip_urb.rs ozma-usb/src/usbip_relay.rs \
  ozma-usb/src/lib.rs ozma-usb/src/error.rs \
  ozma-usb/tests/usbip_urb.rs ozma-usb/tests/usbip_relay.rs
```

## Remaining kernel/dataplane gaps

- There is still no in-tree kernel `usbip-host` stub or `vhci_hcd` adapter. A
  production `UsbIpDataEndpoint` would bridge these URBs to a real kernel USB
  device (or a userspace device backend) and must supply genuine completion
  bytes/statuses.
- The relay is not wired into `UsbIpServer`: a successful `OP_REP_IMPORT` can
  hand the connection to `serve_data_session`, but the node orchestration that
  does so does not exist yet.
- Attach/import success still requires `UsbIpKernelBackend::attach` to return a
  real local port; default deployments remain `usbip-kernel-required`.
- ISO descriptors are validated and round-tripped, but wire-padding compaction
  into a real URB buffer and packet-level scheduling are endpoint duties.
- TLS remains external; the listener and relay are plaintext.
- No live device capture, kernel module loading, sudo, or hardware data-path
  evidence was produced. `ozma-node`, `ozma-multiseat`, and `ozma-audio` were
  not modified.
