# Rust multi-seat planning and supervision (`ozma-multiseat`)

Status: **library + real subprocess supervisor + CLI + controller read-only
grouping + opt-in input-routing/display-ownership execution (2026-11-25).**
The crate enumerates display/input topology, plans one desktop seat per output,
can spawn, health-check, restart, stop, and report one `ozma-node --profile
desktop` child per seat, and models the two ownership layers: `xinput
map-to-output` input routing with rollback, and per-seat `xrandr`
output/mode/position ownership. The supervisor can now **apply those resources
before spawning and release them on stop/remove/shutdown** when the explicit
`--apply-ownership`/`--apply-routing` CLI flags (or the matching
`SupervisorOptions` booleans) are enabled; both default to off, and dry-run
still performs no action. It is **not wired into `ozma-node` or
`ozma-softnode`**, and it creates no virtual display or audio sink. No real
evdev grab is performed and no input is injected. The controller can group the
independent per-seat node registrations for display, without merging them or
taking over seat lifecycle.

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
- `routing`: `InputRouter` (`assign`/`release`/`status`), `XInputRouter` with an
  injectable `CommandRunner` and `DeviceNameResolver`, exact
  `xinput list --short` parsing, `xinput map-to-output <device> <output>`
  construction/execution, release-to-`all`, seat-level rollback, and
  `assign_seats` all-or-nothing assignment. `EvdevGrabRouter` models exclusive
  `EVIOCGRAB` requests and per-seat uinput environment; its default backend
  returns a typed unsupported error instead of grabbing.
- `ownership`: `DisplayOwnership` and `XrandrOwnershipController` model
  per-seat output ownership as
  `xrandr --output <output> --mode <mode> --pos <x>x<y>`, with apply/release
  and rollback through an injectable runner. `SeatPlan` and `NodeCommand`
  environment carry the output, mode, ownership kind, routing strategy, and
  explicit uinput seat.
- Opt-in supervisor resource execution: with `apply_ownership`/`apply_routing`
  enabled, `ProcessSeatSupervisor` applies xrandr ownership and then
  xinput/evdev routing **before** spawning a child. An apply failure prevents
  that spawn, records a typed `SeatResourceError`, and releases resources
  already applied for earlier seats. Stop, remove, rollback, and `Drop` release
  routing before ownership, best effort, with failures kept in status.
- Status carries `resources` (`ownership_applied`, `routing_applied`,
  `ownership_released`, `routing_released`, and typed `errors`). The field is
  omitted when no resource action happened, so default status is unchanged.
- Read-only controller seat grouping (`GET /api/v1/seats`, dashboard Seats
  panel) that recognises the `<machine>-seat-<n>` names the planner produces,
  or explicit `seat_id`/`parent_node_id` metadata, and reports online state,
  seat indexes, node ids, displays, and HID/HTTP ports. It never merges nodes,
  controls seats, or exposes tokens.

Deliberately not implemented yet:

- Node integration inside `ozma-node`/`ozma-softnode`, controller registration,
  or a Python-agent-compatible manager API. The CLI is the external supervisor.
  The controller grouping above is read-only and does not change this.
- **True per-seat screen ownership.** The X11 path positions one xrandr output
  on a single X server. All outputs share one root window and one keyboard
  focus, so `xrandr` ownership is not screen isolation. Separate X screens,
  a per-seat X server/compositor, or DRM leasing are still required and are not
  created here. Wayland compositor seats are modelled only as a kind.
- **Real exclusive evdev grabs.** `EvdevGrabRouter` models `EVIOCGRAB` and
  provides the execution seam, but the default backend is unsupported because
  the crate forbids `unsafe`; no privileged helper exists. udev/Xorg
  `InputDevice` sections are not generated.
- **Automatic routing/ownership execution is opt-in.** The CLI now executes
  the planned ownership/routing commands only with `--apply-ownership` and/or
  `--apply-routing`; both default off, and `--print-plan`/`--dry-run` never
  execute them. There is no policy engine, per-seat retry loop beyond the
  supervisor's restart path, or automatic reconciliation after external X
  layout changes. Evdev routing still requires an injected backend.
- Per-seat uinput targeting at the kernel/compositor level; the node's uinput
  sink still does not choose an X screen. The environment now names the target
  seat, but the node must honour it.
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
| Input routing | `InputRouter`, `XInputRouter`, `EvdevGrabRouter`, `InputTarget`, `InputAssignment`, `InputRelease`, `InputRoutePlan`, `InputRoutingStrategy`, `assign_seats`, `parse_xinput_list`, `XInputDevice`, `RouteCommand`, `CommandRunner`, `ProcessCommandRunner`, `DeviceNameResolver`, `SysfsDeviceNameResolver`, `EvdevGrabBackend`, `EvdevGrabCommand`, `UnsupportedEvdevGrabBackend` |
| Display ownership | `DisplayOwnership`, `DisplayOwnershipKind`, `DisplayOwnershipController`, `XrandrOwnershipController`, `apply_ownership` |
| Plan result | `MultiseatPlan`, `PlanDiagnostic`, `DiagnosticCode` |
| Supervisor | `SeatSupervisor`, `ProcessSeatSupervisor`, `SupervisorOptions`, `NodeLaunchSpec`, `NodeLauncher`, `ReadinessProbe`, `SeatRuntimeStatus`, `SeatResourceStatus`, `SeatResourceError`, `ResourceKind`, `ResourceAction` |
| Status | `MultiseatCapability`, `MultiseatStatus`, `render_metrics` |
| Supervision | `SeatSupervisor`, `ProcessSeatSupervisor`, `SupervisorOptions`, `NodeLaunchSpec`, `NodeLauncher`, `ProcessLauncher`, `ChildHandle`, `ChildExit`, `ReadinessProbe`, `HttpHealthProbe`, `PortProbe`, `restart_delay` |
| CLI | `ozma-multiseat` binary |
| Test-only | `fake::FakeDisplayBackend`, `fake::FakeInputBackend`, `fake::FakeSeatSupervisor`, `fake::FakeNodeLauncher`, `fake::FakeReadinessProbe`, `fake::FakeCommandRunner`, `fake::FakeDeviceNameResolver`, `fake::FakeEvdevGrabBackend` |

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
carries `DISPLAY`, `PULSE_SINK`, `OZMA_SEAT_ID`, `OZMA_X11_OUTPUT`,
`OZMA_X11_MODE`, `OZMA_DISPLAY_OWNERSHIP`, `OZMA_INPUT_ROUTER`, and
`OZMA_UINPUT_SEAT` in the command environment, which the supervisor passes to
the child. For an X11 seat with input, `SeatPlan.ownership` carries the
`DisplayOwnership` and `SeatPlan.input_route` carries the `InputRoutePlan`
(pointer paths, exclusive-grab intent, and routing environment).
`extra_node_args` is appended after the generated flags; callers must not
duplicate an option that clap would reject.

For PipeWire, the capture flags are `--capture-provider pipewire`,
`--desktop-capture-target <configured target>`, and
`--desktop-capture-width/height/fps`. PipeWire seats carry no
`DisplayOwnership`; the compositor owns outputs.

The audio sink and `monitor:<sink>` capture target are **configuration intent**.
Nothing in this crate creates the sink. The node reports a warned no-audio state
if the sink does not exist.

## Input routing and display ownership

These layers turn the plan's device paths and output names into explicit
commands. They are **opt-in** in the CLI and supervisor: with the flags off (the
default) the plan is printed and injected into the child environment only; with
`--apply-ownership`/`--apply-routing` the supervisor calls the
`XrandrOwnershipController` and `XInputRouter` around the child lifecycle. A
caller can still construct a router/controller directly.

### Physical input routing

`InputRouter` is the shared contract:

```rust
fn assign(&mut self, group: &InputGroup, target: &InputTarget) -> Result<InputAssignment, MultiseatError>;
fn release(&mut self, seat_id: &str) -> Result<InputRelease, MultiseatError>;
fn status(&self) -> Vec<InputAssignment>;
```

- `XInputRouter` takes an injected `CommandRunner` and `DeviceNameResolver`. It
  runs `xinput list --short` with `DISPLAY` set to the target's X screen, parses
  `id=<n>` lines (`parse_xinput_list`), resolves each mouse evdev path through
  sysfs (`/sys/class/input/<event>/device/name`), and runs
  `xinput map-to-output <id> <output>`.
- `release` removes the seat assignment and runs
  `xinput map-to-output <id> all` for every mapped pointer (the `all` reset
  value). A partial `assign` failure rolls back earlier pointer maps in reverse
  order before returning the typed `CommandFailed`/`CommandUnavailable`.
- `assign_seats` validates unique seat ids/hubs/outputs before any command,
  assigns in order, and releases every earlier seat on a later failure.
  Keyboard-only groups are tracked with no pointer map; a group whose pointers
  are all absent from XInput is a typed error, never a silent success.
- `EvdevGrabRouter` models `EVIOCGRAB` requests (`EVIOCGRAB_REQUEST`,
  `EvdevGrabCommand`) and builds the per-seat uinput environment. Execution is
  behind `EvdevGrabBackend`; the default `UnsupportedEvdevGrabBackend` returns a
  typed `Unsupported` error. Tests inject `FakeEvdevGrabBackend`, which only
  records requests and never opens `/dev/input`.
- Missing tools are typed `CommandUnavailable` errors. A non-zero `xinput`
  exit is `CommandFailed` with program, argv, status, and stderr.
- XInput names are not unique. Two devices with the same model name resolve to
  the first matching `id`; stable per-device routing for identical devices needs
  udev/Xorg or a device-id-aware backend.

Example argv:

```text
xinput list --short
xinput map-to-output 12 HDMI-1
xinput map-to-output 12 all          # release/reset
```

### Display ownership

`DisplayOwnership` records seat id, kind, X screen, output, mode, geometry, and
primary/virtual flags. `XrandrOwnershipController` applies and releases one
output through an injected runner:

```text
xrandr --output HDMI-1 --mode 1920x1080 --pos 1920x0
xrandr --output HDMI-1 --auto        # best-effort release; prior layout unknown
```

`apply_ownership` rejects duplicate seats/outputs before the first command and
rolls back earlier outputs when a later apply fails. Non-xrandr ownership kinds
(`x_screen`, `separate_server`, `wayland_compositor`) are modelled but return a
typed `Unsupported` error from the xrandr controller.

**Single X server limitation.** All xrandr outputs share one root window, one
window manager, and one keyboard focus. Positioning an output and mapping a
pointer to it is not seat isolation; `ozma-node` still injects into the one X
server. True ownership requires separate X screens (`:0.0`, `:0.1`), one X
server/compositor per seat, or DRM leasing. This crate creates none of those and
claims no Wayland or Windows ownership.

## Supervision

`SeatSupervisor` is the synchronous lifecycle contract:

```rust
fn start(&mut self, plan: &SeatPlan) -> Result<SeatRuntimeStatus, MultiseatError>;
fn stop(&mut self, seat_id: &str) -> Result<SeatRuntimeStatus, MultiseatError>;
fn status(&self) -> Vec<SeatRuntimeStatus>;
```

`ProcessSeatSupervisor` is the real implementation:

- **Resource reservation (opt-in).** With `apply_ownership` and/or
  `apply_routing`, start applies the seat's `xrandr` ownership first, then
  input routing, before spawning. Ownership is applied through
  `XrandrOwnershipController`; xinput seats use `XInputRouter`; an evdev-strategy
  seat needs an injected evdev router, otherwise it fails with a typed
  `Unsupported` error. A failed apply never spawns that seat, records a typed
  `SeatResourceError`, and releases every resource already applied for earlier
  seats in reverse order. Controllers are installed with
  `with_ownership_controller`, `with_routing_controller`, and
  `with_evdev_routing_controller`; without the flags no controller is called.
- **Start.** Spawn `plan.command.program` with its exact args and environment.
  The child inherits the supervisor's stdout/stderr. Wait until `GET /health`
  returns HTTP 200 (default) or a bounded TCP connect succeeds. A readiness
  timeout, probe error, spawn error, or early child exit is a `failed` state,
  never a fabricated `running` state; resources reserved for that failed child
  are released.
- **Restart.** An exited or failed child is restarted with bounded exponential
  backoff (`initial * 2^attempt`, capped). A child that stays up for
  `healthy_uptime` resets the failure count. Already-applied resources remain in
  place across a restart; a resource released after a failed start is re-applied
  before the restart spawn.
- **Stop.** Send `SIGTERM`, wait up to five seconds for the node to exit, then
  kill and reap it. Only after the child is gone are routing and ownership
  released (routing first). Release failures are best effort: the state is
  `stopped`, the applied flag stays set, and the typed error is reported in
  `resources.errors`. `remove` additionally forgets the seat only when its
  resources are released; otherwise it retains the seat for a retry.
- **Status.** Each seat reports pid, planned UDP/HTTP ports, state, restart
  count, next restart delay, the most recent error, and applied/released
  resource state. A planned seat with no runtime entry stays `planned`.
- **Dry run.** `SupervisorOptions::dry_run` records the launch spec and reports
  `planned` with a `dry-run` message; it never spawns, probes, or invokes a
  routing/ownership controller even when the apply flags are set.
- **Tests.** `ProcessSeatSupervisor::with_parts` injects a `NodeLauncher` and a
  `ReadinessProbe`. `FakeNodeLauncher` records spawns, simulates exits and
  spawn failures, and records stop requests; `FakeReadinessProbe` can be ready,
  not-ready, delayed, or failing. `FakeCommandRunner` plus
  `FakeDeviceNameResolver` drive the real `XrandrOwnershipController` and
  `XInputRouter` without running `xrandr`/`xinput`.

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
      "restarts": 0,
      "resources": {
        "ownership_applied": true,
        "routing_applied": true,
        "ownership_released": false,
        "routing_released": false
      }
    }
  ],
  "diagnostics": []
}
```

A failed seat also carries `last_error` and, while a restart is pending,
`next_restart_in_ms`. Resource apply/release failures add entries such as
`{"kind":"ownership","action":"release","message":"..."}` to
`resources.errors`; the field is omitted entirely when both apply flags are off.
`capability.node_count` always equals `capability.seat_count` and the number of
seat entries. There is no merged super-node.

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

## Controller seat grouping (read-only)

The controller still sees one independent node per seat. `controller/seats.py`
derives a display-only grouping at read time and exposes it as
`GET /api/v1/seats`:

- A node is a seat when it has an explicit `seat_id`, its node id (or
  `seat_id`) matches `<machine>-seat-<n>`, or it has a `parent_node_id`. The
  `direct` registration endpoint accepts an optional `seat_id`; older nodes
  that omit it keep working.
- `parent_node_id` names the machine when present and overrides the name
  prefix, so an operator can rename a seat node without splitting the group.
- Nodes that match none of those rules are returned in `unseated_node_ids`.
  A bare `seat-0` or an ordinary `workstation-1` is never reported as a seat.
- Each group carries `machine`/`host`, `seat_count`, `online_count`,
  `offline_count`, `online`, `all_online`, and sorted `seats`. Each seat carries
  `node_id`, `seat_id`, `seat_index`, `online`, `host`, `port`, `api_port`,
  `stream_port`, `display_outputs`/`displays`, and display/role fields when set.
- Online state uses the node's `last_seen` freshness (`offline_after`, default
  180 s). No tokens, node argv, environment, or audio sink names are exposed.
- The dashboard has a read-only Seats panel that groups seat rows under their
  machine; individual node cards remain unchanged. The panel has no start,
  stop, or destructive controls.

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

# Opt in to per-seat xrandr ownership and xinput routing. Both are off by
# default; a failed apply stops the start and releases earlier seats.
ozma-multiseat --display :0 --apply-ownership --apply-routing
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
| `--apply-ownership` | Apply each seat's `xrandr` output ownership before spawn (default off). |
| `--apply-routing` | Apply each seat's xinput/evdev input routing before spawn (default off). |
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

With `--apply-ownership`/`--apply-routing`, the supervisor applies each seat's
resource before spawning it. A missing `xrandr`/`xinput`, a non-zero command, a
non-xrandr ownership kind, or an evdev strategy without a backend is a typed
failure: that seat is not spawned, and already-applied resources for earlier
seats are released. On SIGINT/SIGTERM the CLI calls `stop_all`, which releases
routing/ownership after each child exits. Output lists applied/released state
and any release error; JSON carries `resources` on each seat. With the flags
off (the default) the CLI never runs `xrandr` or `xinput` for ownership/routing,
and `--print-plan`/`--dry-run` never run them even when the flags are present.

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
- XInput list parsing, exact `map-to-output`/list argv, sysfs name resolution,
  partial-map rollback, release-to-`all`, absent-tool typed errors, unknown
  pointer rejection, keyboard-only tracking, and `assign_seats` rollback.
- `EVIOCGRAB` command models, per-seat uinput environment, the unsupported
  default backend, and fake-backend grab/ungrab recording without touching
  `/dev/input`.
- Xrandr ownership argv (`--output/--mode/--pos`), apply/release through a fake
  runner, absent-`xrandr` typed errors, non-xrandr kind rejection, duplicate
  output rejection, and multi-seat apply rollback.
- Supervisor resource execution with fake runner/router/launcher: ownership
  before routing before spawn, apply failure stops the spawn and releases
  earlier seats, stop/remove release routing before ownership, best-effort
  release errors stay in status, dry-run and flags-off perform no resource
  action, missing tools are typed, and an evdev strategy without a backend is
  typed unsupported (`tests/resource_supervisor.rs`).
- Planner wiring of `SeatPlan.ownership`, `SeatPlan.input_route`, and the
  ownership/routing environment.

Controller-side grouping is covered by Python tests with fake nodes:

```bash
python3 -m pytest tests/unit/controller/test_seats.py
```

They cover name/`seat_id`/`parent_node_id` grouping, multi-host separation,
online/offline state, unrelated-node exclusion, the API shape and token
redaction, optional `seat_id` registration compatibility, and dashboard group
rendering.

Real host checks are opt-in and `#[ignore]`d:

```bash
OZMA_MULTISEAT_REAL=1 cargo test -p ozma-multiseat --test real_backends \
  -- --ignored --nocapture
```

They enumerate the real `xrandr` output, read-only sysfs input, and DRM
connectors. They start no node and inject no input.

The routing smoke is also opt-in and can map a pointer only on a throwaway
display (for example Xvfb `:99`):

```bash
OZMA_MULTISEAT_REAL=1 cargo test -p ozma-multiseat --test real_routing \
  -- --ignored --nocapture
OZMA_MULTISEAT_REAL=1 OZMA_MULTISEAT_XINPUT_MAP=1 DISPLAY=:99 \
  cargo test -p ozma-multiseat --test real_routing -- --ignored --nocapture
```

The first check only lists XInput devices. The second calls
`xinput map-to-output` and resets to `all`; it must point at a disposable X
server and never the operator's desktop. No evdev grab or input injection is
part of either check.

Display mode changes are opt-in separately:

```bash
OZMA_MULTISEAT_REAL=1 OZMA_MULTISEAT_XRANDR_APPLY=1 DISPLAY=:99 \
  cargo test -p ozma-multiseat --test real_ownership -- --ignored --nocapture
```

It applies one `--output/--mode/--pos` and releases with `--auto`, again only on
a throwaway display.

The full supervisor resource path is also opt-in:

```bash
Xvfb :97 -screen 0 1280x800x24 &
OZMA_MULTISEAT_REAL=1 OZMA_MULTISEAT_SUPERVISOR_APPLY=1 DISPLAY=:97 \
  cargo test -p ozma-multiseat --test live_resource_supervisor \
  -- --ignored --nocapture
```

It applies real `xrandr` ownership and a real `xinput map-to-output` route, starts
a temporary fake HTTP node, stops it through `ProcessSeatSupervisor`, and
asserts both resources were released. It uses the throwaway display only and
never grabs evdev or injects input.

Live smoke (2026-11-25): `--print-plan`, `--dry-run`, and the normal supervisor
path succeeded against ephemeral Xvfb `:99`/`:97` with read-only sysfs input,
producing the exact `ozma-node --profile desktop` argv without spawning a real
node. The plan printed `ownership: xrandr-output screen mode=1280x800 pos=0x0`
and `input route: xinput-map-to-output ... grab=no`, with
`OZMA_X11_OUTPUT`/`OZMA_X11_MODE`/`OZMA_DISPLAY_OWNERSHIP`/`OZMA_INPUT_ROUTER`/
`OZMA_UINPUT_SEAT` in the child environment. A supervisor smoke with
`--apply-ownership` and a temporary fake `/health` node applied
`xrandr --output screen --mode 1280x800 --pos 0x0` on Xvfb `:97`, reported
`resources(ownership=applied)`, and on SIGTERM stopped/reaped the child and
reported `resources(ownership=released)`. The live
`live_resource_supervisor` test additionally applied both real ownership and a
real `xinput map-to-output 6 screen` route before starting the fake node, then
released the route to `all` and the output with `--auto`; status ended with
`ownership_applied=false`, `routing_applied=false`, both released flags true,
and no resource errors.

The opt-in routing smoke on Xvfb listed six XInput devices and mapped
`Xvfb mouse` (id 6) to the `screen` output with the real
`ProcessCommandRunner`; release reset it with `map-to-output 6 all`. The opt-in
ownership smoke applied `xrandr --output screen --mode 1280x800 --pos 0x0` and
released with `--auto` on the same disposable display. The host's existing X
sockets `:0`–`:2` rejected `xrandr` without Xauthority; the CLI failed with the
backend error rather than inventing a display.

## Remaining Python parity

The Python plan's Phase 1 baseline includes a working seat lifecycle, HTTP/UDP
listeners, registration, capture processes, audio sinks, input assignment, and
hotplug. The Rust crate now covers planning, subprocess supervision, and a
local status surface. Still required for parity:

1. **Node/controller integration.** Wire the supervisor into `ozma-node`,
   `ozma-softnode`, or a manager that registers each seat as an independent
   node with the controller. The controller groups those registrations
   read-only, but it does not start/stop seats, merge them into a super-node,
   or surface supervisor state. The CLI status document is local and
   read-only.
2. **True display ownership.** xrandr output positioning exists as a command
   model/controller, but one X server still shares a root window and keyboard
   focus. Separate X screens, a per-seat X server/compositor, or DRM leasing
   are required. Virtual displays still depend on an external Xorg/dummy driver
   or the future IDD path. There is no Wayland compositor seat path.
3. **Input routing execution and policy.** `xinput map-to-output` and rollback
   are now invoked by the supervisor when `--apply-routing` is enabled, but
   `EVIOCGRAB` still has no privileged grab helper, so evdev stays modelled and
   typed unsupported. udev/Xorg `InputDevice` sections, device permissions,
   hotplug re-routing, and durable per-seat input state are missing.
4. **Per-seat uinput targeting.** The environment now names the target seat
   (`OZMA_UINPUT_SEAT`), but injection still goes to the focused X
   window/server, not a specific seat output or compositor seat.
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
