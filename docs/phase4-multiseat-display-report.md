# Phase 4 — per-seat display isolation report

Date: 2026-11-25
Scope: `ozma-multiseat` only (source, tests, this report, and
`docs/multiseat-rust.md`). No commit was made.

## Problem

The multiseat planner and supervisor could assign one X output per seat, but
every seat still drew on the host's single X root. `xrandr` ownership moved an
output and `xinput map-to-output` moved a pointer; neither isolated the root
window, focus, or application windows. This phase adds a real, opt-in private X
server per seat so a seat's applications can use a distinct `DISPLAY`.

## Backend design

New module: `ozma-multiseat/src/seat_display.rs`.

| Type | Purpose |
|---|---|
| `SeatDisplayBackend` | `shared` (default), `xvfb`, `xephyr`, `xorg`; serialized as snake case. |
| `SeatDisplaySize` | validated `WxH`, parses `--seat-display-size`. |
| `SeatDisplayOptions` | backend, size, number range (default `90..=199`), readiness timeout/poll, optional Xephyr parent, optional Xorg config. |
| `SeatDisplayRequest` / `SeatDisplayHandle` / `SeatDisplayLauncher` | spawn seam; handle exposes `display`, `pid`, `xauthority`, `auth_disabled`, `is_ready`, `try_wait`, `terminate`. |
| `ProcessSeatDisplayLauncher` | production `Xvfb`/`Xephyr`/`Xorg` spawner with a shared `DisplayNumberAllocator`. |
| `DisplayNumberAllocator` | first-free allocation, reuse after release, skips existing X sockets/locks, typed exhaustion. |
| `SeatDisplayStatus` / `SeatDisplayError` / `SeatDisplayNote` | flattened status state and typed failures/notes. |

### Commands

```text
Xvfb :N -screen 0 WxHx24 -nolisten tcp -auth <Xauthority>
# or, when xauth is absent/failed:
Xvfb :N -screen 0 WxHx24 -nolisten tcp -ac

DISPLAY=<shared> Xephyr :N -screen WxHx24 -title ozma-<seat> \
  -nolisten tcp -auth <Xauthority>

Xorg :N -config <file> -logfile <file> -nolisten tcp -noreset \
  -novtswitch -sharevts -keeptty -auth <Xauthority>
# or, when xauth is absent/failed: the same command with -ac
```

`-nolisten tcp` is always present. The `-ac` fallback is therefore limited to
the Unix socket, never a TCP listener. `Xorg` takes its geometry, video driver,
and input devices from the config; it is started with `-novtswitch -sharevts` so
the seat does not steal a VT, `-noreset` so it survives its last client, and
`-keeptty` so the supervisor still owns the child pid. `Xorg` without an
explicit config is a typed `Unsupported` error rather than an implicit host GPU
probe.

### Readiness and auth

Readiness is **not** "the process exists". `is_ready` connects to
`/tmp/.X11-unix/XN`, sends a real X11 connection setup request (optionally with
the random 16-byte `MIT-MAGIC-COOKIE-1`), and requires a `Success` setup reply.
Only then does the supervisor store the handle, report `display_ready=true`, and
export `DISPLAY`/`XAUTHORITY` to the node.

When `xauth` is present the launcher writes a private temporary `Xauthority`
with mode-0700 directory permissions and starts the server with `-auth`. When it
is absent or fails, the server starts with `-ac` and the status carries a typed
`auth_disabled` note.

### Typed errors

| Error | Status kind |
|---|---|
| `SeatDisplayTool` | `missing_binary` |
| `SeatDisplayNumberExhausted` | `number_exhausted` |
| `SeatDisplayStartupTimeout` | `startup_timeout` |
| `SeatDisplayCrashed` | `crashed` |
| `MultiseatError::Unsupported` (Xephyr absent/no parent) | `unsupported` |

`Xephyr` is optional: a missing `Xephyr` is a typed `Unsupported` error. A
missing `Xvfb` or `Xorg` is the typed missing-binary error. A display is never
stored or reported ready after a timeout or crash; the handle is terminated and
the number released first.

## Supervisor integration

`SupervisorOptions` gained `seat_display: SeatDisplayOptions`. The CLI gained:

```text
--seat-display shared|xvfb|xephyr|xorg   (default shared)
--seat-display-size WxH                 (default 1280x800; xorg uses its config)
--seat-display-xorg-config PATH         (required with --seat-display xorg)
```

`ProcessSeatSupervisor::new` installs the production spawner for nested
backends. `with_parts` (tests/CLI) requires
`with_display_launcher(...)`; the CLI installs `ProcessSeatDisplayLauncher`
when the backend is nested.

Lifecycle:

1. **Start:** ensure private display (spawn + X11 handshake) → apply ownership,
   routing, helper isolation, audio → spawn node → wait for node readiness.
   The node launch spec is rewritten: `DISPLAY`, optional `XAUTHORITY`,
   `--x11-display :N`, `--x11-offset 0,0`, `--x11-width/height` from
   `--seat-display-size`. PipeWire capture targets stay unchanged.
2. **Stop:** terminate/reap node → release routing/helper/audio/ownership →
   terminate private X server. Resource release happens while the display is
   still alive; the display is last.
3. **Failure/rollback:** a display failure never spawns the node; a resource or
   readiness failure releases that seat's display. `rollback_applied_resources`
   also releases any private server that has no child. A failed seat keeps a
   typed `display_error` and backoff state.
4. **Crash:** `tick` polls the display. An exit terminates the node, releases
   the server, and schedules a bounded restart. The restart starts a fresh
   server and reuses the released display number.
5. **Shutdown:** `Drop` terminates children first, then displays through the
   rollback path.

### Nested ownership/routing semantics

xrandr output ownership and xinput `map-to-output` refer to physical outputs and
pointers in the shared X server. On a private root they are not executed. The
seat reports:

- `ownership_unsupported` — no physical output to own.
- `xinput_routing_unsupported` — no physical pointer inside the private server.

The applied flags stay false, so status never claims a fake apply. The
privileged helper path is unchanged and still runs before the node: it grabs the
physical devices and reports the per-seat uinput devices even when the display
is private. There is a documented integration gap: those uinput devices are not
yet injected into the private X server/compositor seat.

### Status

For a private display a seat status includes:

```json
"display_backend": "xvfb",
"display": ":90",
"display_pid": 1932048,
"display_ready": true,
"display_notes": [{"kind": "ownership_unsupported", "message": "..."}],
"display_error": {"kind": "startup_timeout", "message": "..."}
```

All fields are omitted when the backend is `shared`. The flat fields are part of
both `SeatRuntimeStatus` and the controller-facing `SeatStatus` (serde
flatten), and the status round-trip test covers them.

## Tests

New `tests/display_supervisor.rs` (15 tests) uses only fakes, plus unit tests in
`seat_display.rs` for the Xorg argument builder and the missing-config refusal:

- default `shared` is a no-op and its status omits private fields;
- `Xvfb` starts before the node, exports `DISPLAY`, rewrites capture args, and
  stops after the node;
- multi-probe readiness;
- startup timeout is typed, releases the number, and spawns no node;
- crash before readiness is typed and releases the number;
- running-display crash stops the node and restarts both with number reuse;
- missing `Xvfb` is a typed missing-binary status error;
- missing `Xephyr` is typed unsupported;
- nested ownership/xinput requests become typed notes with zero commands;
- helper input isolation still applies with a private display;
- resource rollback releases a display that already started;
- cleanup order is display start → node spawn → display stop;
- number allocation/reuse and exhaustion;
- dry-run reports the display plan without spawning;
- display status JSON round-trips.

`FakeSeatDisplayLauncher` in `src/fake.rs` supports ready/timeout/crash/
missing-binary/unsupported/number-exhaustion and records spawns, stops, and
released numbers. It never opens a socket or runs a process.

New opt-in `tests/live_display_smoke.rs` is ignored unless
`OZMA_MULTISEAT_REAL_DISPLAY=1`. It starts two throwaway seats on the first free
numbers at or above `:90`, records each fake node's inherited `DISPLAY`, and
verifies the servers/sockets are gone after `stop_all`. New opt-in
`tests/live_xorg_seat_smoke.rs` is ignored unless
`OZMA_MULTISEAT_REAL_XORG=1` and performs the real-Xorg device/event isolation
above.

## Live smoke evidence

Host tools:

```text
$ command -v Xvfb Xephyr weston xauth xdpyinfo
/usr/bin/Xvfb
/usr/bin/xauth
/usr/bin/xdpyinfo
# Xephyr and weston: not installed
```

Run:

```text
$ OZMA_MULTISEAT_REAL_DISPLAY=1 cargo test -p ozma-multiseat \
    --test live_display_smoke -- --ignored --nocapture
...
MESA: error: ZINK: failed to choose pdev
started smoke-seat-0 display=:90 pid=Some(1932048)
started smoke-seat-1 display=:91 pid=Some(1932161)
recorded DISPLAY values: [":90", ":91"]
PASS: seats=["smoke-seat-0", "smoke-seat-1"] displays=[90, 91] \
  pids=[1932048, 1932161] stopped cleanly
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Post-run cleanup check:

```text
$ ls /tmp/.X11-unix/
X0 X1 X2 X99        # pre-existing sockets; no X90/X91
$ pgrep -a Xvfb
# no output
$ ls -d /tmp/ozma-seat-display-*
# no output
```

The `MESA` messages come from Xvfb probing the host GPU and are harmless; the
private displays themselves passed the X11 handshake. The real desktop (`:0`)
was never used or changed.

## Real Xorg follow-up (2026-11-25)

The Xvfb smoke proved a private X root but could not prove input isolation:
Xvfb exposes only core/XTEST devices, so helper-created uinput nodes were never
enumerated. A second live smoke now starts two real Xorg servers on `:90` and
`:91` with throwaway dummy video and libinput:

```bash
sudo pacman -S --needed --noconfirm xf86-video-dummy xorg-xev
OZMA_MULTISEAT_REAL_XORG=1 \
  cargo test -p ozma-multiseat --test live_xorg_seat_smoke \
  -- --ignored --nocapture
```

The test creates source uinput devices, has the real `ozma-seat-input` helper
grab and relay them, writes a per-display `Xauthority`, starts
`sudo -n /usr/lib/Xorg :90` and `:91` with a generated config (`dummy` video,
libinput `InputDevice` sections, `-nolisten tcp`, `-novtswitch -sharevts
-noreset -keeptty`), applies `ProcessSeatInputTargeter`, and injects keys. It
passed with both displays initially enumerating all four helper devices, seat
A's devices enabled only on `:90`, seat B's only on `:91`, and each key reaching
only its own `xev -root`. Full evidence and cleanup are in
`docs/phase4-xorg-seat-report.md`.

Important config rule discovered live: an explicit Xorg `InputDevice`
identifier must be the helper uinput name (`ozma-seat-...`), otherwise XInput
reports the config's identifier and `--input-targeting` honestly fails. The new
`xorg` backend therefore requires a caller-supplied config and refuses to start
without one.

## Verification

```text
cargo test -p ozma-multiseat
  180 passed, 0 failed, 12 ignored (across lib, bin, and integration tests)

cargo check -p ozma-multiseat --all-targets
  clean (only pre-existing ozma-pipewire warnings)

rustfmt --edition 2021 --check <touched files>
  clean
```

## Remaining gaps

- **Physical output/compositor ownership.** `Xvfb`/`Xephyr` provide a real
  private X root, but there is no DRM leasing, KMS scanout ownership, window
  manager, compositor launch, or physical-output handoff. The private root is a
  virtual display, not a claimed monitor.
- **Input delivery into the private seat.** The helper confirms `EVIOCGRAB` and
  creates per-seat uinput devices. A real Xorg + libinput seat now enumerates
  those devices, and `--input-targeting` restricts each display to its own set;
  the live smoke proves keys do not cross displays. This is still an X11/Xorg
  seat, not a compositor/OS seat: no Wayland compositor, per-seat cgroups,
  physical output handoff, or session manager is launched.
- **Xephyr availability.** Xephyr was not installed on this host, so the nested
  window path is covered by typed-unsupported and command-construction tests,
  not a live window. `weston` was also absent; there is no Wayland compositor
  backend. The real Xorg path was validated instead with dummy video/libinput.
- **Xorg privilege and config.** The distro `Xorg` wrapper allows only console
  users; the live smoke uses `sudo -n /usr/lib/Xorg`. Production still needs a
  privileged launcher/helper and generated Xorg configs with helper uinput
  identifiers. The backend documents this and does not auto-escalate.
- **xauth fallback.** When `xauth` is missing, the server uses `-ac` on a
  Unix-socket-only listener and reports `auth_disabled`. That is honest but
  weaker than a cookie; `-ac` is never exposed over TCP.
- **Physical seat input mapping.** `xinput map-to-output` remains a shared-X
  strategy and is intentionally reported as unsupported on private displays.
  Per-seat physical input for private displays needs the compositor/uinput
  integration above.
- **Node/controller integration.** The private display is prepared by the
  supervisor; `ozma-node`/`ozma-softnode` still need to consume the seat's
  private display and input devices as one session, and the controller still
  groups seats read-only.
