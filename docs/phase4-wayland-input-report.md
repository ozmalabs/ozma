# Phase 4 — per-seat Wayland synthetic input injection report

Date: 2026-11-26
Scope: `ozma-multiseat` (`seat_input`, `seat_display`, `supervisor`, `main`,
`fake`, new unit/integration/live tests, `docs/multiseat-rust.md`, and this
report). No commit was made. `ozma-node`, `ozma-usb`, `ozma-mesh`, and the
controller Python were not touched.

## Result

Per-seat synthetic input is implemented and proven with two real headless
`sway` compositors. The injector connects only to the requested seat's private
`WAYLAND_DISPLAY` socket, creates a virtual keyboard with an XKB keymap plus a
virtual pointer, and types `a` (keycode 38) into seat A and `b` (keycode 56)
into seat B. Each probe received only its own keycode.

```text
seat A events:
KEYBOARD
CONFIGURED
ENTER
KEY 38 Value(Pressed)
KEY 38 Value(Released)

seat B events:
CONFIGURED
KEYBOARD
ENTER
KEY 56 Value(Pressed)
KEY 56 Value(Released)

PASS: seat A received keycode 38 only, seat B received keycode 56 only
```

This is **synthetic input isolation**, not physical input isolation. No
uinput device is created, no `/dev/input` node is opened, and helper uinput
devices are not attached to the headless compositor. The status keeps the
physical `input_isolation_applied`/`input_targeting_applied` flags false.

## Protocol and compositor setup

Installed only because the proof needs a wlroots compositor that implements
the two virtual-input globals:

```bash
sudo pacman -S --needed --noconfirm sway
sway --version
# sway version 1.12
```

The host already had `wayland-info`, `libwayland-client`, and `libxkbcommon`.
Sway exposes:

```text
zwp_virtual_keyboard_manager_v1  version 1
zwlr_virtual_pointer_manager_v1  version 2
wl_seat                          version 9
```

The virtual keyboard protocol is not in `wayland-protocols`; the code uses
`wayland-protocols-misc` for `zwp_virtual_keyboard_manager_v1` and
`wayland-protocols-wlr` for `zwlr_virtual_pointer_manager_v1`, on top of
`wayland-client` 0.31. The US keymap is compiled once and embedded from
`ozma-multiseat/data/wayland-us.xkb`; injection does not depend on the host's
XKB data or `xkbcomp`.

Each seat is a throwaway compositor:

```text
XDG_RUNTIME_DIR=<throwaway>/runtime
WLR_BACKENDS=headless
WLR_RENDERER=pixman
WLR_LIBINPUT_NO_DEVICES=1
WLR_NO_HARDWARE_CURSORS=1
WLR_HEADLESS_OUTPUTS=1
WAYLAND_DISPLAY removed from the inherited environment
sway --config <throwaway>/sway.conf
```

Generated `sway.conf`:

```ini
output * resolution 1280x800
default_border none
xwayland disable
```

Sway ignores an inherited `WAYLAND_DISPLAY` and creates `wayland-1` in the
private runtime dir. The launcher waits for that socket and creates
`ozma-sway-N` as a symlink inside the same private runtime dir, so the seat has
a stable private display name. The socket path is therefore
`<runtime>/ozma-sway-N`, unreachable from another seat's runtime dir.

## Implementation

New module: `ozma-multiseat/src/seat_input.rs`.

| Type | Purpose |
|---|---|
| `SeatInputMode` | `off` (default) / `wayland`; CLI value is `--seat-input`. |
| `SeatInputRequest` | `seat_id` + the seat's `SeatWaylandEnv`. |
| `SeatInputCapabilities` | keyboard/pointer availability and protocol names. |
| `SeatInputInjector` | Injectable seam: `connect`, `inject_key`, `inject_text`, `pointer_motion`, `pointer_button`, `disconnect`, `events_sent`, `connected_seats`. |
| `WaylandSeatInputInjector` | Real `wayland-client` implementation. |
| `SeatInputError` | Typed `protocol_unavailable`, `compositor_refused`, `disconnected`, `not_connected`, `unsupported_key`, `io`, `unsupported`. |
| `FakeSeatInputInjector` | Test fake that records key/text/motion/button events by seat and never opens a socket. |

Wire behavior:

1. Connect to `SeatWaylandEnv.socket` with a `UnixStream` (never the ambient
   environment), then wrap it with `Connection::from_socket`.
2. Bind `wl_seat`, `zwp_virtual_keyboard_manager_v1`, and
   `zwlr_virtual_pointer_manager_v1`. A missing seat or either virtual-input
   manager is a typed `ProtocolUnavailable`; no partial keyboard-only
   connection is left behind under the real injector.
3. Create the virtual keyboard and call `keymap(1, fd, size)` with the
   compiled-in US XKB keymap, then round-trip. The round-trip is the point at
   which a compositor refusal becomes `CompositorRefused`.
4. Create a virtual pointer and send relative `motion`, `button`, and (for
   protocol v2) `frame` requests.
5. `inject_text` maps US characters with `ascii_key`; `"a"` is XKB keycode
   `38` (evdev 30 + 8) and `"b"` is `56` (evdev 48 + 8). Unsupported
   characters return `UnsupportedKey`.
6. Flush after every event batch. A closed connection maps to `Disconnected`.

New binary: `ozma-multiseat/src/bin/ozma-wayland-key-probe.rs`. It is the
minimal Wayland client used by the live test: it binds `wl_seat`/`wl_keyboard`,
maps one xdg-shell toplevel with a zero-filled shm buffer, and prints `ENTER`
and `KEY <keycode> <state>`.

## Supervisor / CLI integration

`SupervisorOptions::seat_input` defaults to `Off`.

- `ProcessSeatSupervisor::new` installs `WaylandSeatInputInjector` when the
  mode is `wayland`; the CLI (`with_parts`) installs it explicitly; tests inject
  `FakeSeatInputInjector`.
- After the private display is ready, `apply_seat_input` connects to that
  seat's `SeatWaylandEnv` only. It records:
  - `resources.input_injection_available` only when both protocol globals were
    bound;
  - `resources.input_injection_applied` only when the keyboard and connection
    exist;
  - `resources.input_injection_events` on disconnect.
- A missing protocol, refused compositor, absent injector, or non-Wayland
  display records a typed `seat_input` resource error plus an
  `input_injection_unavailable` display note. The flags stay false; the seat
  still starts (input is synthetic/opt-in, not the seat's physical input).
- Stop, display crash, rollback, and `Drop` disconnect before the compositor is
  removed and set `input_injection_released`.
- `--seat-input wayland` requires `--seat-display sway`. It is rejected with a
  typed validation error for `shared`/X backends and for `weston`, because
  Weston does not expose the wlroots virtual-pointer global.

CLI:

```bash
ozma-multiseat --display :0 --seat-display sway --seat-input wayland \
  --seat-display-size 1280x800
```

Status example (fake-backed supervisor test):

```json
{
  "display_backend": "sway",
  "display": "ozma-sway-90",
  "display_ready": true,
  "resources": {
    "input_injection_applied": true,
    "input_injection_available": true
  }
}
```

Status never claims `input_isolation_applied` or `input_targeting_applied` from
this path. On a protocol failure the same document has both injection flags
false and `display_notes[].kind == "input_injection_unavailable"`.

## Live test and evidence

Command:

```bash
OZMA_MULTISEAT_REAL_SWAY=1 \
  cargo test -p ozma-multiseat --test live_sway_input_smoke \
  -- --ignored --nocapture
```

Recorded output:

```text
started sway-input-seat-0 display=ozma-sway-90 \
  runtime=/tmp/ozma-seat-sway-2532837-0/runtime \
  socket=/tmp/ozma-seat-sway-2532837-0/runtime/ozma-sway-90 pid=Some(2532839)
started sway-input-seat-1 display=ozma-sway-91 \
  runtime=/tmp/ozma-seat-sway-2532837-1/runtime \
  socket=/tmp/ozma-seat-sway-2532837-1/runtime/ozma-sway-91 pid=Some(2532844)
seat A protocol caps: SeatInputCapabilities {
  keyboard: true, pointer: true,
  keyboard_protocol: Some("zwp_virtual_keyboard_manager_v1"),
  pointer_protocol: Some("zwlr_virtual_pointer_manager_v1") }
seat B protocol caps: SeatInputCapabilities {
  keyboard: true, pointer: true,
  keyboard_protocol: Some("zwp_virtual_keyboard_manager_v1"),
  pointer_protocol: Some("zwlr_virtual_pointer_manager_v1") }
seat A events:
KEYBOARD
CONFIGURED
ENTER
KEY 38 Value(Pressed)
KEY 38 Value(Released)

seat B events:
CONFIGURED
KEYBOARD
ENTER
KEY 56 Value(Pressed)
KEY 56 Value(Released)

PASS: seat A received keycode 38 only, seat B received keycode 56 only;
  displays=["/tmp/ozma-seat-sway-2532837-0/runtime/ozma-sway-90",
            "/tmp/ozma-seat-sway-2532837-1/runtime/ozma-sway-91"]
test result: ok. 1 passed; 0 failed; 0 ignored
```

The test also sends relative pointer motion and a primary-button press/release
on seat A. Both requests succeeded through `zwlr_virtual_pointer_manager_v1`;
the probe asserts the keyboard routing (the observable focus path) rather than
claiming a pointer-focus assertion it does not perform.

Post-run cleanup:

```text
$ ls -d /tmp/ozma-seat-sway-*
# no output
$ pgrep -af 'sway --config /tmp/ozma-seat-sway'
# no ozma sway process
```

Only throwaway compositors were used. The host's real Wayland/X session was
never queried or modified.

## Testing

Fake-backed and unit coverage (default suite, no compositor/socket):

- `tests/sway_input_supervisor.rs` (5 tests): two seats get only their own
  `ozma-sway-N` socket and runtime dir; `input_injection_available/applied`
  status; stop disconnect/released state; typed missing-protocol failure with
  both flags false and an `input_injection_unavailable` note; shared-display
  refusal without a request; fake per-seat key/text/pointer isolation.
- `seat_input` unit tests: mode parsing, US/XKB keycode mapping
  (`a=38`, `b=56`, uppercase/numbers/symbols), stable typed error kinds, and
  the embedded keymap shape.
- Existing display/supervisor tests were updated only for the new
  `SupervisorOptions::seat_input` field.

Verification on the working tree:

```text
cargo test -p ozma-multiseat
  241 passed, 0 failed, 18 ignored

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
  clean
```

The extra passing tests come from unrelated concurrent DRM writeback work in
the same working tree. The same package tests were also run in a detached
worktree at `HEAD` plus this change, where the result was 230 passed / 0
failed / 16 ignored. `cargo fmt -p ozma-multiseat -- --check` reports formatting
diffs only in unrelated in-progress `src/drm_lease.rs` and DRM writeback
sections of `src/fake.rs`; this change's own files have no rustfmt diff.

The live smoke was run separately in the working tree and passed
(`1 passed`, 0.94 s). The default suite does not start a compositor or open
`/dev/input`.

## Remaining gaps / honest blockers

- **Not physical input isolation.** The headless compositor still has no
  libinput seat. A real physical keyboard/pointer (or helper uinput node) is
  not attached to it. The implementation proves per-compositor synthetic input
  routing; it does not prove that a physical device is confined to one seat.
- **No global uinput fallback.** This is deliberate: when the virtual-input
  protocols are missing, the injector reports `protocol_unavailable` and the
  status stays false. Weston lacks the wlroots pointer global, so that backend
  is rejected for `--seat-input wayland`.
- **Pointer delivery is not asserted by the live probe.** The live test proves
  both virtual protocols bind and accepts pointer motion/button requests; the
  focused client assertion is keyboard-only.
- **No ad-hoc CLI injection command.** Injection is exposed as the documented
  library seam (`SeatInputRequest` + `SeatInputInjector`); the supervisor only
  establishes/reports the per-seat connection. There is no
  `ozma-multiseat inject` subcommand yet.
- **Sway socket naming.** Sway creates `wayland-N`; the stable `ozma-sway-N`
  name is a symlink inside the private runtime dir. It is private and cleaned
  with the runtime dir, but it is not the name sway itself chose.
- **Node capture/automation.** `ozma-node` does not consume the seat's
  `WAYLAND_DISPLAY` for capture and does not call the injector. This work is
  supervisor/library/test-only.
- **Physical output/compositor ownership** remains out of scope: headless sway
  renders to memory and does not hand a monitor or DRM lease to the seat.
