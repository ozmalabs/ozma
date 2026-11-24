# Rust multi-seat planning and supervision (`ozma-multiseat`)

Status: **library + real subprocess supervisor + CLI (2026-11-25).** The crate
enumerates display/input topology, plans one desktop seat per output, and can
spawn, health-check, restart, stop, and report one `ozma-node --profile desktop`
child per seat. It is **not wired into `ozma-node` or `ozma-softnode`**, creates
no virtual display or audio sink, grabs no input device, and injects no input.

The Python baseline in [`agent/multiseat`](../agent/multiseat) remains the
runnable multi-seat implementation. The plan and phase status are in
[`multi-seat-plan.md`](multi-seat-plan.md); the desktop node contract is in
[`desktop-profile.md`](desktop-profile.md).

## Scope

Implemented:

- Display output model and `DisplayBackend` trait.
- Linux `xrandr --listactivemonitors` enumeration for an **explicit** X display,
  with a pure parser and a binary/display detection helper.
- DRM connector detection from `/sys/class/drm` (status and advertised modes).
  This is detection only, not a KMS/scanout or mode-setting backend.
- Input device model and `InputBackend` trait.
- Linux sysfs/evdev enumeration: capability-bit classification (keyboard,
  mouse, gamepad) and USB-hub tracing, without opening an evdev node.
- USB-hub grouping (`internal` first, then hubs lexicographically).
- `SeatConfig` and `SeatPlan` with display, input paths, audio sink, ports,
  profile, capture transport, command argv, and environment.
- Deterministic planning: one seat per display, sequential port allocation,
  input-group assignment, gamepad-only assignment, and non-fatal diagnostics
  for missing input, extra input groups, and a display cap.
- Explicit X11 or PipeWire capture selection. PipeWire targets are supplied by
  the operator per display; the planner never invents a node name.
- `HostTopology`, which joins one display backend and one input backend and
  refuses an empty display list or a backend error instead of fabricating a
  seat or input device.
- `ProcessSeatSupervisor`: real per-seat `ozma-node` child spawn with the
  planned argv/environment, readiness wait on `/health` (or a bounded TCP
  connect), bounded exponential restart backoff, stop/reap, and per-seat
  pid/ports/state/restarts/last-error reporting.
- `NodeLauncher` and `ReadinessProbe` seams. Production uses
  `ProcessLauncher` and `HttpHealthProbe`; tests inject `FakeNodeLauncher` and
  `FakeReadinessProbe` and never execute a process.
- `ozma-multiseat` CLI: enumerate X11/DRM/input topology, print or validate the
  plan, run a no-spawn dry run, or supervise the seats. It serves optional
  read-only `/health`, `/status`, `/metrics`, and `/api/v1/connection`
  documents on `--status-port` (loopback only).
- Controller-facing capability/status document where one seat is one node,
  plus a bounded Prometheus metrics renderer.

Deliberately not implemented yet:

- Node integration inside `ozma-node`/`ozma-softnode`, controller registration,
  or a Python-agent-compatible manager API. The CLI is the external supervisor.
- Xorg per-seat screens or output ownership; the X11 plan captures a region of
  one existing X server.
- Physical input routing to a seat (`xinput map-to-output`, exclusive
  `EVIOCGRAB`, udev/Xorg `InputDevice` sections).
- Per-seat uinput targeting; the node's uinput sink does not choose an X screen.
- Audio sink creation and per-process/per-seat routing.
- Hotplug/auto-detection, persistence, scale up/down, and virtualization of
  missing displays beyond the `DisplayBackend::create_virtual` contract.
- OS isolation/resource control for children: cgroups/namespaces, user
  switching, CPU/GPU allocation, process-group/subreaper teardown, log
  files/rotation, restart-count limits, or supervision of a node that has no
  HTTP listener.
- GPU/encoder allocation, game launcher, WebRTC seats, and the Python
  monitoring endpoints.
- Any Windows/macOS path. There is **no** DXGI, IDD, Raw Input, WASAPI, or
  XInput parity claim here.

## Public API

| Area | Types / functions |
|---|---|
| Display | `DisplayOutput`, `DisplayBackend`, `XrandrDisplayBackend`, `parse_listactivemonitors`, `BackendDetection` |
| DRM | `drm::detect`, `drm::detect_at`, `DrmConnector`, `DrmStatus`, `DrmDetection` |
| Input | `InputDevice`, `InputDeviceKind`, `InputGroup`, `InputBackend`, `SysfsInputBackend`, `group_inputs`, `classify_capabilities`, `usb_hub_path_from_sysfs` |
| Host | `HostTopology` |
| Planning | `PlannerOptions`, `CapturePreference`, `MultiseatPlanner`, `allocate_ports` |
| Seat | `SeatProfile`, `ProfileDefaults`, `SeatConfig`, `SeatPlan`, `CapturePlan`, `NodeCommand`, `SeatPorts`, `SeatState`, `SeatStatus`, `SeatRuntimeStatus` |
| Plan result | `MultiseatPlan`, `PlanDiagnostic`, `DiagnosticCode` |
| Status | `MultiseatCapability`, `MultiseatStatus`, `render_metrics` |
| Supervision | `SeatSupervisor`, `ProcessSeatSupervisor`, `SupervisorOptions`, `NodeLaunchSpec`, `NodeLauncher`, `ProcessLauncher`, `ChildHandle`, `ChildExit`, `ReadinessProbe`, `HttpHealthProbe`, `PortProbe`, `restart_delay` |
| CLI | `ozma-multiseat` binary |
| Test-only | `fake::FakeDisplayBackend`, `fake::FakeInputBackend`, `fake::FakeSeatSupervisor`, `fake::FakeNodeLauncher`, `fake::FakeReadinessProbe` |

`fake` is not selected by detection and never touches `/dev`, sysfs, X11,
PipeWire, or processes. Production code must choose `XrandrDisplayBackend` /
`SysfsInputBackend` / `ProcessLauncher` explicitly.

The crate has `#![forbid(unsafe_code)]`. The Unix stop path uses the safe `nix`
signal API rather than an `unsafe` libc call.

## Planning model

`MultiseatPlanner::plan` consumes already-enumerated displays and devices;
`plan_with_backends` enumerates through the backend traits and may call
`DisplayBackend::create_virtual` when the requested seat count exceeds the
physical output count.

1. Requested seats are `PlannerOptions::seat_count`, or one per display when it
   is `None`. `Some(0)` is a typed error.
2. Missing displays are filled by `create_virtual` when a backend supports it.
   Otherwise the plan is capped and gains `RequestedSeatsCapped` /
   `VirtualDisplayUnavailable` diagnostics. No display means
   `MultiseatError::NoDisplays`, never a synthetic seat.
3. Displays are sorted by `(index, name)`; duplicate names or indexes are
   rejected. Zero-sized outputs are rejected.
4. One seat is assigned to each display. Seat ids and node names are
   `<machine>-seat-<n>`; audio sinks are `<prefix>-<n>` (default
   `ozma-seat-<n>`).
5. Input is grouped by USB hub. Full groups (keyboard or mouse) fill seats in
   group order; the `internal` group comes first. Gamepad-only groups attach to
   seats in order. Surplus groups and seats without input are reported as
   diagnostics, not silently dropped.
6. Ports are allocated deterministically: seat `n` gets `base_udp + n` and
   `base_http + n` (defaults `7331` / `7382`). Overflow is a typed error.
7. Capture comes from the seat profile defaults plus `CapturePreference`:
   - `X11` uses the output's `x_screen` and `x`/`y`/`width`/`height`, or
     `PlannerOptions::x11_display` as an explicit fallback.
   - `PipeWire` requires a target for each display name/index in
     `CapturePreference::PipeWire::targets`; a missing target is a typed error.

The default generated argv is ordered and stable. A two-seat X11 host produces,
for seat 0:

```text
ozma-node --profile desktop --name host-seat-0 \
  --hid-udp-port 7331 --http-port 7382 --input-provider uinput \
  --capture-provider x11 --x11-display :0 --x11-offset 0,0 \
  --x11-width 1920 --x11-height 1080 --x11-fps 30 \
  --audio-sink ozma-seat-0 --audio-capture-target monitor:ozma-seat-0
```

and seat 1 increments the name, ports, sink, and display offset. The plan also
carries `DISPLAY`, `PULSE_SINK`, and `OZMA_SEAT_ID` in the command environment,
which the supervisor passes to the child. `extra_node_args` is appended after
the generated flags; callers must not duplicate an option that clap would
reject.

For PipeWire, the capture flags are `--capture-provider pipewire`,
`--desktop-capture-target <configured target>`, and
`--desktop-capture-width/height/fps`.

The audio sink and `monitor:<sink>` capture target are **configuration intent**.
Nothing in this crate creates the sink. The node reports a warned no-audio state
if the sink does not exist.

## Supervision

`SeatSupervisor` is the synchronous lifecycle contract:

```rust
fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError>;
fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError>;
fn status(&self) -> Vec<SeatRuntimeStatus>;
```

`ProcessSeatSupervisor` is the real implementation:

- **Start.** Spawn `plan.command.program` with its exact args and environment.
  The child inherits the supervisor's stdout/stderr. Wait until `GET /health`
  returns HTTP 200 (default) or a bounded TCP connect succeeds. A readiness
  timeout, probe error, spawn error, or early child exit is a `failed` state,
  never a fabricated `running` state.
- **Restart.** An exited or failed child is restarted with bounded exponential
  backoff (`initial * 2^attempt`, capped). A child that stays up for
  `healthy_uptime` resets the failure count. There is no restart-count limit;
  call `stop`/`remove` to end supervision.
- **Stop.** Send `SIGTERM`, wait up to five seconds for the node to exit, then
  kill and reap it. `remove` additionally forgets the seat; `stop` keeps the
  stopped status so it can be started again.
- **Status.** Each seat reports pid, planned UDP/HTTP ports, state, restart
  count, next restart delay, and the most recent error. A planned seat with no
  runtime entry stays `planned`.
- **Dry run.** `SupervisorOptions::dry_run` records the launch spec and reports
  `planned` with a `dry-run` message; it never spawns or probes.
- **Tests.** `ProcessSeatSupervisor::with_parts` injects a `NodeLauncher` and a
  `ReadinessProbe`. `FakeNodeLauncher` records spawns, simulates exits and
  spawn failures, and records stop requests; `FakeReadinessProbe` can be ready,
  not-ready, delayed, or failing.

`ProcessSeatSupervisor::tick` performs child polling and due restarts. It is
synchronous; a caller with an async runtime runs it from a poll loop. `Drop`
stops any child still owned by the supervisor.

## Status shape

`MultiseatStatus` is built from a plan and optional supervisor statuses:

```json
{
  "capability": {
    "available": true,
    "seat_count": 2,
    "node_count": 2,
    "display_backend": "xrandr",
    "input_backend": "sysfs",
    "capture_providers": ["x11"]
  },
  "requested_seats": 2,
  "seats": [
    {
      "seat_id": "host-seat-0",
      "node_name": "host-seat-0",
      "display_index": 0,
      "display_output": "HDMI-1",
      "udp_port": 7331,
      "http_port": 7382,
      "audio_sink": "ozma-seat-0",
      "profile": "desktop",
      "capture": {
        "kind": "x11",
        "display": ":0",
        "offset_x": 0,
        "offset_y": 0,
        "width": 1920,
        "height": 1080,
        "fps": 30
      },
      "input_devices": ["/dev/input/event0", "/dev/input/event1"],
      "state": "running",
      "pid": 4242,
      "restarts": 0
    }
  ],
  "diagnostics": []
}
```

A failed seat also carries `last_error` and, while a restart is pending,
`next_restart_in_ms`. `capability.node_count` always equals
`capability.seat_count` and the number of seat entries. There is no merged
super-node.

`render_metrics` emits a bounded text surface:

```text
ozma_multiseat_nodes 2
ozma_multiseat_seats{state="running"} 2
ozma_multiseat_seat_up{seat="host-seat-0"} 1
ozma_multiseat_seat_restarts_total{seat="host-seat-0"} 0
ozma_multiseat_seat_port{seat="host-seat-0",kind="udp"} 7331
ozma_multiseat_seat_port{seat="host-seat-0",kind="http"} 7382
```

Metrics labels are seat ids and fixed state/port kinds only. Node argv,
environment, input paths, audio sink names, and error strings stay out of the
metrics surface.

## CLI

```bash
# Enumerate and print the plan (no spawn).
ozma-multiseat --display :0 --print-plan

# Validate the launch plan, including dry-run state, as JSON.
ozma-multiseat --display :0 --dry-run --json

# Start and supervise one node per display; status on loopback:7390.
ozma-multiseat --display :0 \
  --base-port 7331 --base-http-port 7382 \
  --status-port 7390 \
  --restart-backoff-ms 1000 --restart-backoff-max-ms 30000 \
  --readiness-timeout-ms 15000 --poll-interval-ms 1000
```

Configuration:

| Flag | Meaning |
|---|---|
| `--node-bin PATH` | Node binary (default `ozma-node`). |
| `--base-port N` | First HID UDP port; seat N gets `N + seat`. |
| `--base-http-port N` | First HTTP port; seat N gets `N + seat`. |
| `--status-port N` | Loopback status/metrics listener; `0` disables. |
| `--restart-backoff-ms`, `--restart-backoff-max-ms` | Bounded restart delays. |
| `--readiness-timeout-ms`, `--readiness-poll-ms` | Readiness budget and probe interval. |
| `--poll-interval-ms` | Child poll/restart tick. |
| `--healthy-uptime-secs` | Uptime that resets the failure count. |
| `--print-plan` | Print the plan and exit without spawning. |
| `--dry-run` | Record/print launch state without spawning or probing. |
| `--machine-name NAME` | Seat id prefix. |
| `--profile NAME` | Seat profile (capture defaults/label). |
| `--seats N` | Requested seats (default one per display). |
| `--display DISPLAY` | Explicit X display (default ambient `DISPLAY`). |
| `--readiness health\|port` | `/health` HTTP check or TCP connect only. |
| `--health-path PATH` | Health endpoint (default `/health`). |
| `--json` | JSON plan/status output. |

The CLI aborts if `xrandr` is missing, the X display cannot be opened, no
display is reported, or input enumeration fails. It does not fall back to a
fake backend. DRM connectors are printed for diagnostics only. A seat with no
input group is still planned with a `SeatWithoutInput` diagnostic.

The optional status listener is bound to `127.0.0.1` and exposes only
`/health`, `/status`, `/metrics`, and `/api/v1/connection`. It carries no
bearer tokens, node argv, environment, or operator secret.

## Tests

```bash
cargo test -p ozma-multiseat
cargo check -p ozma-multiseat --all-targets
cargo fmt -p ozma-multiseat -- --check
```

The default suite covers:

- Xrandr parser (multiple monitors, virtual/negative offsets, malformed output).
- DNS-free DRM fixtures (connected/disconnected/unknown connectors, modes).
- sysfs fixtures and hub tracing, capability classification, detection.
- Grouping by hub, internal-first ordering, gamepad-only assignment.
- Sequential ports, overflow, exact default argv, profiles, X11/PipeWire
  capture selection and fallbacks.
- One seat per display, duplicate display rejection, display caps, virtual
  displays, missing input, extra input groups, backend failure propagation.
- `HostTopology` no-display and backend-failure refusal.
- Status JSON shape and round-trip, one node per seat, runtime state merge,
  fake supervisor behavior.
- Plan-to-spawn argv/environment/ports, readiness delay, readiness timeout and
  probe errors, spawn failure retry, crash restart with backoff, stop/reap,
  remove, dry-run no-spawn, per-seat pid/ports/restarts/last-error JSON, and
  bounded metrics.

Real host checks are opt-in and `#[ignore]`d:

```bash
OZMA_MULTISEAT_REAL=1 cargo test -p ozma-multiseat --test real_backends \
  -- --ignored --nocapture
```

They enumerate the real `xrandr` output, read-only sysfs input, and DRM
connectors. They start no node and inject no input.

Live smoke (2026-11-25): `--print-plan` and `--dry-run` succeeded against an
ephemeral Xvfb `:99` with read-only sysfs input, producing one seat and the
exact `ozma-node --profile desktop` argv without spawning a node or injecting
input. A separate supervisor smoke used a temporary fake `/health` node: the
real `ProcessLauncher` passed `DISPLAY`, `PULSE_SINK`, and `OZMA_SEAT_ID`,
readiness succeeded, `/status`/`/metrics` reported pid/ports/state, and
SIGTERM stopped and reaped the child. No real `ozma-node` was started. The
host's existing X sockets `:0`–`:2` rejected `xrandr` without Xauthority; the
CLI failed with the backend error rather than inventing a display.

## Remaining Python parity

The Python plan's Phase 1 baseline includes a working seat lifecycle, HTTP/UDP
listeners, registration, capture processes, audio sinks, input assignment, and
hotplug. The Rust crate now covers planning, subprocess supervision, and a
local status surface. Still required for parity:

1. **Node/controller integration.** Wire the supervisor into `ozma-node`,
   `ozma-softnode`, or a manager that registers each seat as an independent
   node with the controller. The CLI status document is local and read-only.
2. **Display ownership.** Separate X screens or a compositor path per seat;
   current X11 planning captures a region of one server. Virtual displays still
   depend on an external Xorg/dummy driver or the future IDD path.
3. **Physical input routing.** `xinput map-to-output`, exclusive evdev grabs,
   permissions/udev rules, and per-seat input state. The plan records device
   paths but does not enforce ownership.
4. **Per-seat uinput targeting.** Injection currently goes to the focused X
   window/server, not a specific seat output.
5. **Audio isolation.** Create PipeWire null/remap sinks per seat, route
   application streams, and add mic/room-correction/VBAN paths.
6. **Auto-detection.** USB hotplug, seat persistence (`seats.json`), hub
   add/remove, seat scale up/down, and virtual-display-on-demand.
7. **Profiles and games.** Launcher commands/environment, Steam/Lutris/Playnite
   discovery, per-seat GPU/encoder allocation, and isolation (`user`,
   `sandboxie`, namespaces, cgroups).
8. **Per-seat streaming.** HLS/WebRTC endpoints, per-seat monitoring, and the
   controller seeing N independent registrations.
9. **Supervisor hardening.** Process-group/subreaper teardown, log
   capture/rotation, restart-count limits, readiness for non-HTTP nodes, child
   resource limits, and integration with systemd/process supervision.
10. **Windows.** DXGI/SetupAPI display enumeration, IddCx virtual displays, DXGI
    Desktop Duplication per output, Raw Input routing, WASAPI endpoints, and
    XInput gamepad mapping are **not implemented or claimed**.
11. **macOS.** Not claimed.

The Python `agent/multiseat` package remains the reference for behavior and is
not being deleted by this crate.
