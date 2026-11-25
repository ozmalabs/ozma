# Phase 4 — headless Weston per-seat Wayland display report

Date: 2026-11-25
Scope: `ozma-multiseat` (`seat_display`, `supervisor`, `main`, fake launcher,
unit/integration tests, one opt-in live test, `docs/multiseat-rust.md`, and this
report). No commit was made. `ozma-node`, `ozma-usb`, `ozma-mesh`, and the
controller Python were not touched.

## Result

`--seat-display weston` now starts one throwaway headless Weston compositor per
seat, on a private `XDG_RUNTIME_DIR` and a unique `WAYLAND_DISPLAY` socket
(`ozma-weston-<N>`). The seat child receives `WAYLAND_DISPLAY` and
`XDG_RUNTIME_DIR`; inherited `DISPLAY`/`XAUTHORITY` are removed so it cannot
attach to the operator's X session. Two live compositors were validated with the
real `wayland-info` client, and a client using seat A's runtime could not reach
seat B's socket. Both seats were stopped and their sockets, runtime dirs, config
files, and pids were gone.

The headless backend has no libinput seat, so input targeting/isolation is **not**
claimed: the seat records a typed `input_targeting_unavailable` note and keeps
`input_targeting_applied=false`.

## Host probe and package

Before this work `weston` was absent; `wayland-info` and `weston-terminal` were
already present.

```text
$ command -v weston weston-terminal wayland-info weston-info
# weston: missing
/usr/bin/weston-terminal
/usr/bin/wayland-info
# weston-info: missing
```

`weston` was installed only because the backend requires it:

```bash
sudo pacman -S --needed --noconfirm weston
# installed: weston 15.0.1 (pacman also pulled libgusb and colord as deps)
$ weston --version
weston 15.0.1
```

No display manager, session, or existing compositor was changed. The live test
never opens DRM, mode-sets a GPU, or connects to the host's real Wayland/X
session.

## Backend design

New pieces in `ozma-multiseat/src/seat_display.rs`:

| Type / function | Purpose |
|---|---|
| `SeatDisplayBackend::Weston` | `weston`/`wayland` backend name; `is_nested()` and `is_wayland()` are true. |
| `SeatWaylandEnv` | `WAYLAND_DISPLAY`, private `XDG_RUNTIME_DIR`, and the absolute socket path. |
| `wayland_display_name(N)` | `ozma-weston-N`. |
| `weston_config(size)` | Generated per-seat `weston.ini`. |
| `weston_server_args(...)` | Pure argv builder used by unit tests. |
| `probe_wayland_display(env)` | Socket connect plus a real `wayland-info` client round-trip when available. |
| `WestonDisplayHandle` | Child process, private runtime dir, readiness, crash polling, idempotent stop, and cleanup. |
| `SeatDisplayPrograms.weston` | Overridable binary (default `weston`); missing path is a typed error. |

Generated command per seat:

```text
XDG_RUNTIME_DIR=<throwaway>/runtime \
weston --backend=headless --renderer=pixman \
  --socket=ozma-weston-N --idle-time=0 \
  --config=<throwaway>/weston.ini --width=W --height=H
```

The config is minimal and non-interactive:

```ini
[core]
idle-time=0

[shell]
panel-position=none
background-color=0x000000
```

The explicit `--renderer=pixman` keeps the seat software/headless-compatible: no
GPU, GL, Vulkan, or DRM device is required. The throwaway directory contains the
runtime dir, config, socket, and Weston's lock file. It is mode-restricted and
removed on stop, crash cleanup, startup failure, and rollback.

### Readiness and allocation

A spawned process is not a ready display. Readiness:

1. the socket path must exist;
2. a real Unix connection to the Wayland socket must succeed; and
3. when `wayland-info` is installed, that client is run with the seat's private
   `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY` and must exit successfully.

Only then does the supervisor store the handle and report `display_ready=true`.
The display/socket number comes from the existing `DisplayNumberAllocator`
(default `:90..=199` naming space), so a stopped/rolled-back seat reuses its
number. Because the socket lives in a per-seat runtime dir, `ozma-weston-N`
cannot collide with the operator's compositor.

### Typed failures

| Failure | Typed error | Status kind |
|---|---|---|
| `weston` absent/not executable | `SeatDisplayTool { backend: "weston" }` | `missing_binary` |
| process exits before readiness | `SeatDisplayCrashed` | `crashed` |
| process never accepts a client | `SeatDisplayStartupTimeout` | `startup_timeout` |
| exhausted number range | `SeatDisplayNumberExhausted` | `number_exhausted` |
| non-Unix host | `Unsupported` | `unsupported` |

## Supervisor integration

- **Start:** allocate → create runtime dir/config → spawn Weston → probe → export
  `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR` → apply opt-in resources/audio → spawn
  node. A display failure never spawns the node.
- **Stop:** terminate the node → release resources/targeting → terminate Weston
  → delete its runtime directory → release the number.
- **Rollback:** a failed lease/resource apply or node readiness terminates and
  removes the compositor. `rollback_applied_resources` also releases any private
  display with no child.
- **Crash:** `tick` polls the compositor. An exit stops the node, clears
  targeting state, removes the runtime dir, releases the number, and schedules a
  bounded restart. The restart gets a fresh compositor and reuses the number.
- **Shutdown:** `Drop` terminates children first, then displays through the
  rollback path.

### Environment and isolation

For an X backend the child gets `DISPLAY` (and `XAUTHORITY`). For Weston it gets:

```text
WAYLAND_DISPLAY=ozma-weston-N
XDG_RUNTIME_DIR=/tmp/ozma-seat-weston-<pid>-<seq>/runtime
```

`DISPLAY` and `XAUTHORITY` are removed. The planned X11 capture target is not
rewritten to the Wayland name; node-side Wayland capture remains a gap (a
PipeWire capture target would be unaffected).

### Input caveat

Headless Weston creates no libinput seat, so it cannot receive the helper's
uinput devices. When helper isolation is requested on this backend, the helper
still grabs the physical devices and confirms its uinput names, but:

- the display status carries `input_targeting_unavailable`;
- `resources.input_targeting_applied` stays false;
- `targeted_devices`/`disabled_devices` stay empty; and
- `xinput` is never invoked for that seat.

This is deliberate. A compositor with a real libinput seat (or a future
`weston --backend=drm`/virtual-input integration) is required before per-seat
Wayland input can be claimed.

### Status example

```json
"display_backend": "weston",
"display": "ozma-weston-90",
"display_pid": 727024,
"display_ready": true,
"display_notes": [
  {"kind": "input_targeting_unavailable", "message": "..."}
]
```

All fields are omitted for the default `shared` backend.

## Tests

New fake-backed `tests/weston_display_supervisor.rs` (9 tests):

- default `shared` remains a no-op and omits private status fields;
- Weston starts before the node, exports `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`,
  removes `DISPLAY`/`XAUTHORITY`, and stops after the node;
- multi-probe readiness;
- startup timeout is typed, releases the number, and spawns no node;
- crash before readiness is typed and releases the number;
- running-compositor crash stops the node and restarts both with number reuse;
- missing `weston` is a typed `missing_binary` status error;
- resource-apply rollback releases an already-started compositor;
- helper isolation with Weston records `input_targeting_unavailable`, makes zero
  targeting requests, and never claims targeting.

New unit tests in `seat_display.rs` cover the backend name/alias, headless argv
(no DRM/X flags, pixman, private socket/config), generated config, Wayland env
socket join, and missing-binary allocation cleanup.

`FakeSeatDisplayLauncher` now returns a `SeatWaylandEnv` when the request backend
is `Weston`, so supervisor tests can assert env propagation without a process.

New opt-in `tests/live_weston_smoke.rs` (ignored unless
`OZMA_MULTISEAT_REAL_WAYLAND=1`) starts two real Weston compositors, runs
`wayland-info` against each, checks cross-runtime refusal, and asserts cleanup.

## Live smoke evidence

```text
$ OZMA_MULTISEAT_REAL_WAYLAND=1 \
    cargo test -p ozma-multiseat --test live_weston_smoke \
    -- --ignored --nocapture
...
Using Pixman renderer
Output 'headless' enabled with head(s) headless
started smoke-seat-0 display=ozma-weston-90 \
  socket=/tmp/ozma-seat-weston-727022-0/runtime/ozma-weston-90 pid=Some(727024)
started smoke-seat-1 display=ozma-weston-91 \
  socket=/tmp/ozma-seat-weston-727022-1/runtime/ozma-weston-91 pid=Some(727028)
smoke-seat-0 client connected: 2410 bytes of protocol output
smoke-seat-1 client connected: 2410 bytes of protocol output
cross-runtime isolation confirmed: /tmp/ozma-seat-weston-727022-0/runtime \
  -> ozma-weston-91 refused (failed to create display: No such file or directory)
caught signal 15
caught signal 15
PASS: displays=["ozma-weston-90", "ozma-weston-91"] \
  sockets=[...] pids=[727024, 727028] stopped cleanly
test result: ok. 1 passed; 0 failed; 0 ignored
```

`wayland-info` reported `wl_compositor` and the headless output in both seats.
Post-run cleanup:

```text
$ ls -d /tmp/ozma-seat-weston-*
# no output
$ pgrep -af 'weston --backend=headless'
# no weston process
```

The live run used only the throwaway compositors. The host's real session was
not queried or modified.

## Verification

```text
cargo test -p ozma-multiseat
  194 passed, 0 failed, 13 ignored (across lib, bin, and integration tests)

OZMA_MULTISEAT_REAL_WAYLAND=1 cargo test -p ozma-multiseat \
  --test live_weston_smoke -- --ignored --nocapture
  1 passed (0.15 s)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
  clean
```

## Remaining gaps

- **Physical output/compositor ownership.** Weston runs headless to memory. No
  DRM lease/scanout, KMS ownership, physical monitor handoff, or session manager
  is created. A user on the real desktop still sees nothing from these seats.
- **Wayland input isolation.** The headless backend has no libinput seat, so the
  helper's uinput devices cannot be attached to this compositor. Input isolation
  is explicitly unclaimed. A future real-seat backend would need to expose the
  helper's devices through libinput or a virtual-input protocol.
- **Node capture.** `ozma-node` does not yet consume `WAYLAND_DISPLAY` for
  screen capture. The seat launches with the variables above, but capture
  integration is unchanged and out of scope; X11 capture targets are not
  rewritten to a Wayland name.
- **CLI planning still enumerates X displays.** The `ozma-multiseat` CLI still
  uses `xrandr`/an explicit X display to build seat plans. The Weston backend
  replaces the seat's private display, not the planning input. A future plan
  source could enumerate DRM connectors or virtual outputs for a purely
  Wayland-hosted deployment.
- **Session/seat policy.** No per-seat cgroup, namespace, logind seat, or session
  manager is created; the compositor is a bare throwaway process.
- **No input injection or physical input test.** No physical device was opened
  or grabbed by this work. The live smoke used only Wayland protocol clients.
