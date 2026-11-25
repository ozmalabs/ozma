# Phase 4 multi-seat input isolation report

Status: **opt-in privileged input isolation plus private-display targeting
implemented (2026-11-25).**
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
- With a private `--seat-display xvfb|xephyr`, `--input-targeting warn|strict`
  makes the private X server consume only the seat's helper uinput devices and
  disables other seats' `ozma-seat-...` devices. Core/XTEST devices are left
  alone.
- Isolation is never reported as applied unless the helper confirms every grab
  and every uinput device; targeting is never reported applied unless
  `xinput` confirms every own device. A targeting failure is a typed error and
  a visible warning (default) or a prevented spawn (strict).

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
{"event":"grabbed","seat_id":"host-seat-0","grabbed":["/dev/input/event3"],"uinput_devices":["ozma-seat-host-seat-0-kbd-event3"]}
{"event":"released","seat_id":"host-seat-0","released":["/dev/input/event3"]}
{"event":"status","seats":[{"seat_id":"host-seat-0","grabbed":["/dev/input/event3"],"uinput_devices":["ozma-seat-host-seat-0-kbd-event3"]}]}
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
- uinput devices are named `ozma-seat-<seat>-<kbd|mouse|pad|other>-<eventN>`
  (bounded below the kernel's 80-byte limit). The capability token lets a
  display targeter recognise a seat's keyboard/mouse/pad devices without parsing
  a hyphenated seat id. Names mirror the source's key, relative, absolute, and
  switch capabilities. Force feedback, LEDs, and exotic event types are not
  mirrored; this is documented as a limitation.

### Fake backend

`--backend fake` uses the library `FakeHelperBackend`. It records requests,
creates no device, and reports `backend: "fake"`. The supervisor client refuses
a fake readiness response unless `HelperClientConfig::allow_fake_backend` is
set, which only the process-protocol test does.

## Supervisor integration

Library modules: `src/input_isolation.rs` (protocol types, validation, helper
state machine, `SeatInputHelper` trait, `HelperClient`) and
`src/input_targeting.rs` (`SeatInputTargeter`, `ProcessSeatInputTargeter`,
request/result types, `InputTargetingMode`, bounded real `xinput` runner).

CLI:

```text
--input-isolation xinput|helper          (default xinput)
--input-helper PATH                      (default ozma-seat-input)
--input-targeting off|warn|strict        (default warn)
--seat-display shared|xvfb|xephyr        (default shared)
```

`helper` is mutually exclusive with `--apply-routing`: a grabbed device cannot
also be shared through `xinput map-to-output`.

Lifecycle:

1. `helper` rewrites each seat's `InputRoutePlan` to the
   `seat-input-helper` strategy and sets `grab_exclusive = true`.
2. Start applies ownership first (if requested), then the confirmed helper grab,
   then private-display targeting when the display is `xvfb`/`xephyr`, then
   audio. Routing is skipped when isolation owns input.
3. A missing helper, failed/partial grab, or cross-seat conflict prevents the
   spawn, records a typed `{"kind":"input_isolation"}` apply error, and releases
   resources applied for earlier seats.
4. After the helper grab, `ProcessSeatInputTargeter` lists devices with `xinput
   list --short` on the seat's `DISPLAY`/`XAUTHORITY`. It validates every own
   helper uinput name, disables other `ozma-seat-...` devices, then enables its
   own names. Core/XTEST and non-ozma devices are untouched. Disable/enable
   failures roll back the devices already changed and return a typed
   `{"kind":"input_targeting"}` error. `warn` (default) records the error and a
   `input_targeting_failed` note but still spawns; `strict` releases the helper
   and display and prevents the spawn; `off` skips with a typed
   `input_targeting_unavailable` note. The shared display is always a no-op.
5. Stop/remove/rollback/`Drop` release targeting (re-enabling other seats'
   devices) before isolation, routing, and ownership. A release failure keeps
   its applied flag for retry and is recorded in status. The private display is
   stopped last; its teardown clears targeting state without running commands
   against a dead server. When the last isolation seat is released, the helper
   is shut down; the next start launches it lazily again.
6. `tick` re-checks every applied seat. If the helper restarted or lost the
   grab, the applied flag/count and targeting are cleared, a typed error is
   recorded, and a fresh grab plus targeting apply are attempted. A display
   crash clears targeting so the replacement display is re-targeted. If that
   fails, an unisolated running child is terminated and scheduled for restart.

Status (`resources`) gains:

- `input_isolation_applied`
- `input_isolation_released`
- `input_targeting_applied`
- `input_targeting_released`
- `grabbed_device_count`
- `uinput_devices` (helper-confirmed names)
- `targeted_devices` (own names enabled on the private display)
- `disabled_devices` (other-seat names disabled there)
- typed errors with `kind: "input_isolation"` or `"input_targeting"` and
  `action: "apply"|"release"`

The fields are omitted when the corresponding resource never ran, so default
status JSON is unchanged. Nothing is redacted: the document is loopback-only and
intended for diagnosis.

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
- `tests/input_targeting.rs` (7 pass):
  - `xinput list` classification and exact `disable`/`enable` argv with
    `DISPLAY`/`XAUTHORITY` environment;
  - only other `ozma-seat-...` devices are disabled; core devices are untouched;
  - a missing own device fails before any mutation;
  - release re-enables other-seat devices and is idempotent;
  - disable/enable failure rollback;
  - explicit `off`/`warn`/`strict` parsing.
- `tests/input_targeting_supervisor.rs` (9 pass):
  - display start → helper grab → target → node spawn order;
  - own devices enabled, cross-seat devices disabled in status;
  - strict failure prevents spawn and rolls back helper/display/earlier seats;
  - warn failure still spawns with a typed error and `input_targeting_failed`
    note, without claiming targeting;
  - default shared display is a targeting no-op;
  - stop re-enables other-seat devices and clears status;
  - helper restart and display restart re-apply targeting without a stale claim;
  - typed targeting release failure.

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

Opt-in throwaway-display targeting smoke:

```bash
OZMA_MULTISEAT_REAL_TARGETING=1 \
  cargo test -p ozma-multiseat --test live_input_targeting_smoke \
  -- --ignored --nocapture
```

It starts one private `Xvfb` at `:90`, lists devices with the real `xinput`,
targets an existing device through `ProcessSeatInputTargeter`, verifies a
missing `ozma-seat-...` name is a typed `input_targeting` error, releases, and
stops the server. It creates no evdev/uinput device and never touches the
operator's desktop.

Live evidence (2026-11-25): the smoke passed against Xvfb `:90`; `xinput` saw 6
devices, the targeter enabled `Virtual core pointer`, the missing-device case
reported `uinput device(s) not visible on :90`, and the server stopped with no
`Xvfb` process or `X90` socket left behind.

## Verification

```text
cargo test -p ozma-multiseat
  163 passed, 0 failed, 10 ignored (incl. the opt-in live targeting smoke)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
  clean
```

The default run starts no X server, runs no real `xinput`, opens no evdev node,
and performs no grab. The live targeting smoke above is the only run that starts
a throwaway display and invokes the real `xinput`; it creates no uinput device
and uses only `:90` under `-nolisten tcp`.

## Remaining OS-isolation gaps

1. **Compositor/OS seat consumption.** The helper creates per-seat uinput devices
   with stable kind-prefixed names and the supervisor now restricts a private X
   display to those names with `xinput`. That is not the same as an OS/compositor
   seat: `Xvfb` itself exposes only core/XTEST devices, so a live private
   `Xvfb` targeting attempt honestly fails (`input_targeting_failed`). A real
   Xorg/libinput seat, Wayland compositor seat, compositor device selection, or
   DRM lease is still required. `ozma-node`/`ozma-softnode` also do not yet
   consume the names as the seat's input source, so physical events reach the
   uinput nodes but end-to-end seat input depends on the display stack exposing
   them.
2. **Device permissions.** `ozma-seat-input` needs read/write access to the
   configured evdev nodes and `/dev/uinput`. There is no installer/udev policy
   in this task; operators must grant capabilities (preferred) or group access.
   The helper is privileged by design and validates the protocol input.
3. **udev/Xorg policy.** No `InputDevice` Xorg sections are generated, and the
   helper uses sysfs/evdev grouping only. The `xinput` targeter assumes a display
   has already enumerated the helper's uinput nodes; it does not configure the
   Xorg/libinput/compositor device backend that would do so. Hotplug re-grabbing
   is driven by the supervisor's sysfs poll, not udev/netlink.
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
