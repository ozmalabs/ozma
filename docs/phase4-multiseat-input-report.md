# Phase 4 multi-seat input isolation report

Status: **opt-in privileged input isolation implemented (2026-11-25).**
Scope: `ozma-multiseat/` plus this report. No commit was made by this task.

## Problem closed

Before this change, multi-seat input was plan-only: the supervisor could run
`xinput map-to-output`, but every seat still shared physical evdev nodes in the
same X server. The modelled `EvdevGrabRouter` returned a typed `Unsupported`
error because the library forbids `unsafe` and no privileged grab helper
existed.

This change adds the missing execution path without weakening the default:

- `--input-isolation xinput` is the default and is byte-for-byte the previous
  behaviour.
- `--input-isolation helper` starts a privileged helper that opens the seat's
  `/dev/input/event*` nodes, issues `EVIOCGRAB`, creates per-seat uinput
  devices, and relays events.
- Isolation is never reported as applied unless the helper confirms every grab
  and every uinput device.

## Helper: `ozma-seat-input`

Source: `src/bin/ozma-seat-input/`.

- `main.rs`: stdio JSON protocol, request loop, SIGTERM/SIGINT handling.
- `real.rs`: real evdev/uinput backend (`Device::grab`, `VirtualDeviceBuilder`,
  relay threads, capability mirroring).
- `ioctl.rs`: the binary's only `unsafe` module. It sets `O_NONBLOCK` on the
  grabbed fd so relay threads can observe their stop flag; `EVIOCGRAB` itself is
  issued by the safe `evdev` crate.

### Protocol

One JSON object per line on stdin/stdout. Version 1.

Requests:

```json
{"op":"hello","protocol":1}
{"op":"grab","seat_id":"host-seat-0","devices":[{"path":"/dev/input/event3","kind":"keyboard"}]}
{"op":"release","seat_id":"host-seat-0"}
{"op":"status"}
{"op":"shutdown"}
```

Responses:

```json
{"event":"ready","protocol":1,"helper":"ozma-seat-input","backend":"real","pid":4242}
{"event":"grabbed","seat_id":"host-seat-0","grabbed":["/dev/input/event3"],"uinput_devices":["ozma-seat-host-seat-0-event3"]}
{"event":"released","seat_id":"host-seat-0","released":["/dev/input/event3"]}
{"event":"status","seats":[{"seat_id":"host-seat-0","grabbed":["/dev/input/event3"],"uinput_devices":["ozma-seat-host-seat-0-event3"]}]}
{"event":"shutting_down"}
{"event":"error","error":{"code":"cross_seat_conflict","message":"device ... is already grabbed for another seat","seat_id":"host-seat-1","device":"/dev/input/event3"}}
```

Error codes: `invalid_request`, `invalid_path`, `duplicate_device`,
`cross_seat_conflict`, `seat_already_grabbed`, `grab_failed`, `uinput_failed`,
`not_grabbed`, `backend`, `protocol`.

### Safety rules

- Only lexical `/dev/input/event<digits>` paths are accepted. By-id paths,
  `..`, relative paths, other device classes, and implausibly long event
  numbers are rejected before any backend call. The real backend also rejects
  symlinked nodes.
- Duplicate paths in one request are rejected.
- The entire request is validated before the first grab. A device already
  grabbed for another seat is a typed `cross_seat_conflict`; a different set for
  an already-grabbed seat is `seat_already_grabbed`. Repeating the same set is
  idempotent.
- Grab order is grab → create uinput → start relay. Any failure rolls back the
  current step and every completed device in reverse order before responding.
- Release is idempotent (unknown seats return an empty `released` list) so a
  supervisor rollback can safely retry after a helper restart.
- SIGTERM/SIGINT are blocked in the main thread and consumed by a safe `sigwait`
  thread, which releases every seat and exits. EOF and `shutdown` do the same.
- uinput devices are named `ozma-seat-<seat>-<eventN>` (bounded below the
  kernel's 80-byte limit) and mirror the source's key, relative, absolute, and
  switch capabilities. Force feedback, LEDs, and exotic event types are not
  mirrored; this is documented as a limitation.

### Fake backend

`--backend fake` uses the library `FakeHelperBackend`. It records requests,
creates no device, and reports `backend: "fake"`. The supervisor client refuses
a fake readiness response unless `HelperClientConfig::allow_fake_backend` is
set, which only the process-protocol test does.

## Supervisor integration

Library module: `src/input_isolation.rs` (protocol types, validation, helper
state machine, `SeatInputHelper` trait, `HelperClient`).

CLI:

```text
--input-isolation xinput|helper   (default xinput)
--input-helper PATH               (default ozma-seat-input)
```

`helper` is mutually exclusive with `--apply-routing`: a grabbed device cannot
also be shared through `xinput map-to-output`.

Lifecycle:

1. `helper` rewrites each seat's `InputRoutePlan` to the
   `seat-input-helper` strategy and sets `grab_exclusive = true`.
2. Start applies ownership first (if requested), then the confirmed helper grab,
   then audio. Routing is skipped when isolation owns input.
3. A missing helper, failed/partial grab, or cross-seat conflict prevents the
   spawn, records a typed `{"kind":"input_isolation"}` apply error, and releases
   resources applied for earlier seats.
4. Stop/remove/rollback/`Drop` release isolation before routing and ownership.
   A release failure keeps `input_isolation_applied` set and retains the seat so
   `remove` can retry. When the last isolation seat is released, the helper is
   shut down; the next start launches it lazily again.
5. `tick` re-checks every applied seat. If the helper restarted or lost the
   grab, the applied flag/count are cleared, a typed error is recorded, and a
   fresh grab is attempted. If that fails, an unisolated running child is
   terminated and scheduled for restart.

Status (`resources`) gains:

- `input_isolation_applied`
- `input_isolation_released`
- `grabbed_device_count`
- `uinput_devices` (helper-confirmed names)
- typed errors with `kind: "input_isolation"` and `action: "apply"|"release"`

The fields are omitted when isolation never ran, so default status JSON is
unchanged. Nothing is redacted: the document is loopback-only and intended for
diagnosis.

## Tests

Default suite (no real device, no root, no real grab):

```bash
cargo test -p ozma-multiseat
cargo check -p ozma-multiseat --all-targets
rustfmt --edition 2021 --check <touched files>
```

New coverage:

- `tests/input_isolation.rs` (13 pass, 1 ignored):
  - protocol JSON round-trip;
  - ready/grab/status/release/shutdown state machine with
    `FakeHelperBackend`;
  - strict path rejection before any backend call;
  - duplicate, cross-seat, and changed-set rejection;
  - grab, uinput, and release failure rollback/retry;
  - real subprocess protocol against `ozma-seat-input --backend fake`;
  - fake-backend refusal without explicit opt-in.
- `tests/input_isolation_supervisor.rs` (9 pass):
  - confirmed grab before spawn and grabbed-count/uinput status;
  - stop release and exact device paths;
  - grab failure prevents spawn and releases earlier seats;
  - helper readiness failure and a missing helper both prevent spawn with typed
    errors;
  - release failure keeps the assignment and seat for retry;
  - helper crash detection and re-grab without respawning the node;
  - cross-seat conflict typed start failure;
  - dry-run starts no helper; default options leave isolation off.

Existing tests still prove the default `EvdevGrabRouter` returns a typed
`Unsupported` error and that the default supervisor path never invokes a grab.

Opt-in real-device test:

```bash
OZMA_MULTISEAT_REAL_INPUT_GRAB=1 \
OZMA_MULTISEAT_INPUT_DEVICE=/dev/input/eventN \
  cargo test -p ozma-multiseat --test input_isolation \
  real_helper_grabs_and_releases_the_configured_device -- --ignored --nocapture
```

It requires permission to open/`EVIOCGRAB` that node and `/dev/uinput`, grabs
exactly the configured device, verifies one uinput name, and releases it.

## Remaining OS-isolation gaps

1. **Node/compositor consumption.** The helper creates per-seat uinput devices
   and reports their names, but `ozma-node`/`ozma-softnode` do not yet consume
   those names as the seat's input source. Physical events are relayed to
   uinput, but a single X server still has one keyboard focus; separate X
   servers/screens, a compositor seat, or DRM leasing are still required for
   true seat input ownership.
2. **Device permissions.** `ozma-seat-input` needs read/write access to the
   configured evdev nodes and `/dev/uinput`. There is no installer/udev policy
   in this task; operators must grant capabilities (preferred) or group access.
   The helper is privileged by design and validates the protocol input.
3. **udev/Xorg policy.** No `InputDevice` Xorg sections are generated, and the
   helper uses sysfs/evdev grouping only. Hotplug re-grabbing is driven by the
   supervisor's sysfs poll, not udev/netlink.
4. **Capability mirroring.** Keys, relative/absolute axes, switches, and
   standard input events are mirrored with a default ABS range. Force feedback,
   LEDs, and exotic event types are not forwarded. A relay thread that fails
   after a successful grab/uinput creation (for example a uinput write error) is
   not yet surfaced separately to the supervisor; the helper process/assignment
   checks still detect a crash or loss but not a silent relay error.
5. **Durable input state.** Seat/device grabs live in the helper process; a
   supervisor restart replans and re-grabs, and a helper crash is repaired on
   tick, but there is no persisted grab journal across a full host reboot.
6. **Physical recovery.** SIGTERM/EOF/shutdown restore devices; SIGKILL of the
   helper also releases grabs when the kernel closes its fds, but no explicit
   recovery message is emitted. A hard power loss relies on kernel cleanup.
7. **Per-seat application routing.** Audio has null sinks; input isolation has
   virtual devices. Moving applications onto a seat is still outside this
   supervisor (no cgroups, namespaces, or per-seat session manager).
