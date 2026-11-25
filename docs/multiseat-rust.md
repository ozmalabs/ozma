# Rust multi-seat planning and supervision (`ozma-multiseat`)

Status: **library + real subprocess supervisor + CLI + controller read-only
grouping + persisted seats/hotplug reconciliation + opt-in
input-routing/display-ownership/audio execution + opt-in privileged input
isolation + private-display input targeting + opt-in per-seat X servers +
opt-in DRM lease-only handoff + packaged systemd unit (2026-11-25).**
The crate enumerates display/input topology, plans one desktop seat per output,
can spawn, health-check, restart, stop, and report one `ozma-node --profile
desktop` child per seat, and models the opt-in resource layers: `xinput
map-to-output` input routing with rollback, per-seat `xrandr`
output/mode/position ownership, per-seat PipeWire/PulseAudio null sinks, a
privileged `ozma-seat-input` helper that performs real `EVIOCGRAB` and per-seat
uinput creation, `xinput` targeting that restricts a private display to one
seat's helper devices, and opt-in `Xvfb`/`Xephyr`/`Xorg` private X servers per
seat. The real-Xorg path was validated with dummy video, libinput, per-seat
`Xauthority`, two throwaway displays, and XInput event routing; see
`phase4-xorg-seat-report.md`. The supervisor **applies those resources before
spawning and releases them on stop/remove/shutdown** when the explicit
`--apply-ownership`/`--apply-routing`/`--audio-router`/`--input-isolation
helper`/`--input-targeting`/`--seat-display` CLI flags (or the matching
`SupervisorOptions`/builder calls) are enabled; all default to off, and dry-run
still performs no action. The default input path remains the xinput strategy;
the helper path is never selected implicitly. The default display backend
remains `shared` (one X root); `xvfb`/`xephyr`/`xorg` start a real private
server only when requested. A versioned `seats.json` store (`--seats-file`)
preserves seat
ids, display outputs, input hubs, ports, and audio sinks across replans; a sysfs
hotplug model (`--hotplug`) reconciles add/remove display and input-hub events
into seat start/stop with debounce. `ozma-node` can load that same store with
`--multiseat --seats-file` and expose the persisted plan/status read-only at
`GET /api/v1/seats`; it **does not** own seat lifecycle (start/stop/restart stay
with this standalone supervisor). `ozma-softnode` has no seat integration. The
helper grabs physical devices only when explicitly requested and confirmed; it
is not invoked by the default plan, tests, or dry runs. The controller can
consume each node's seat report when present, group the independent per-seat
node registrations for display, and fall back to the historical name/metadata
grouping for older nodes without merging them or taking over seat lifecycle.

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
- **DRM leasing** (`drm_lease`): typed sysfs/`/dev/dri` card enumeration, object
  inspection, `DRM_IOCTL_MODE_CREATE_LEASE` through the maintained `drm` crate,
  an owned lessee fd, `GET_LEASE` validation, revoke/close on drop, and typed
  no-DRM/no-permission/unsupported/unknown-object/already-leased/
  capability-required errors. `AutoVirtual` only selects a `vkms` card; a real
  card/output is selected only by naming `/dev/dri/cardN`. The feature is
  opt-in and **lease-only**: it does not create scanout or compositor
  isolation.
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
- **Privileged input isolation** (`input_isolation` + `ozma-seat-input`): a
  line-delimited JSON protocol and helper state machine with strict
  `/dev/input/eventN` validation, duplicate/cross-seat rejection, and rollback.
  The helper binary is the only component that opens evdev/uinput, calls
  `EVIOCGRAB`, creates per-seat uinput devices, and relays events; its single
  unsafe module sets `O_NONBLOCK` on a grabbed fd. `HelperClient` is the
  non-privileged supervisor client and refuses a fake helper unless a test
  explicitly opts in. Helper uinput names carry the seat, capability class, and
  source event node (`ozma-seat-<seat>-<kbd|mouse|pad|other>-<eventN>`).
- **Private-display input targeting** (`input_targeting`): once a private
  `Xvfb`/`Xephyr`/`Xorg` display is ready and the helper has confirmed its grab,
  `ProcessSeatInputTargeter` runs `xinput list --short` on that seat's
  `DISPLAY`/`XAUTHORITY`, enables the seat's own helper uinput names, and
  disables every other `ozma-seat-...` device. Core pointer/keyboard/XTEST
  devices and unrelated devices are never touched. The real runner bounds each
  `xinput` invocation. `InputTargetingMode::Warn` (default) records a typed
  `input_targeting` error and continues; `Strict` prevents the spawn; `Off`
  never runs `xinput`. Targeting is only claimed after every own device was
  found and enabled, and the disabled devices are exposed in status. A real
  Xorg seat with libinput exposes the helper's uinput nodes and passes this
  check; `Xvfb` exposes only core/XTEST devices and fails honestly.
- `ownership`: `DisplayOwnership` and `XrandrOwnershipController` model
  per-seat output ownership as
  `xrandr --output <output> --mode <mode> --pos <x>x<y>`, with apply/release
  and rollback through an injectable runner. `SeatPlan` and `NodeCommand`
  environment carry the output, mode, ownership kind, routing strategy, and
  explicit uinput seat.
- Opt-in supervisor resource execution: with `apply_ownership`/`apply_routing`
  enabled, `ProcessSeatSupervisor` applies xrandr ownership and then
  xinput/evdev routing **before** spawning a child. With `input_isolation` it
  instead asks the helper for a confirmed grab of the seat's complete device
  set. An apply failure prevents that spawn, records a typed
  `SeatResourceError`, and releases resources already applied for earlier seats.
  Stop, remove, rollback, and `Drop` release isolation, routing, and ownership,
  best effort, with failures kept in status.
- Status carries `resources` (`ownership_applied`, `routing_applied`,
  `input_isolation_applied`, `input_targeting_applied`, `audio_applied`,
  matching `*_released` flags, `grabbed_device_count`, `uinput_devices`,
  `targeted_devices`, `disabled_devices`, and typed `errors`). The field is
  omitted when no resource action happened, so default status is unchanged.
- **Opt-in per-seat displays** (`seat_display`): `shared` (default, one X root),
  `xvfb` (private `Xvfb :N -screen 0 WxHx24 -nolisten tcp` per seat),
  `xephyr` (private X server nested as a window in the shared display), and
  `xorg` (a real Xorg server with a caller-supplied config and libinput; it needs
  the host's Xorg privilege rules and refuses to start without a config). The
  supervisor allocates a free display number, creates a per-display `xauth`
  cookie (or falls back to `-ac` on the Unix-socket-only listener), completes a
  real X11 connection setup handshake before exporting `DISPLAY`/`XAUTHORITY`
  to the child, rewrites X11 capture flags to the private screen, and stops the
  server after the node on stop/rollback/shutdown. Display state is reported as
  `display_backend`, `display`, `display_pid`, `display_ready`, typed
  `display_error`, and typed `display_notes`.
- **Seat persistence** (`persistence`): a versioned `SeatStore`/`seats.json`
  with explicit seat id, display output, input hub, device paths, UDP/HTTP
  ports, audio sink, and profile. Loads validate ids/displays/ports/sinks and
  migrate unversioned files; newer versions are rejected. Saves are atomic
  (same-directory temp file, fsync, rename). `MultiseatPlanner::plan_with_store`
  reuses a persisted identity only while its display is discovered, allocates
  the first unused id/port pair for new displays, and emits
  `PersistedSeatMissingDisplay` / `PersistedSeatMissingInput` diagnostics
  instead of resurrecting missing hardware. Missing-display entries stay in the
  store for replug.
- **Hotplug reconciliation** (`hotplug`): a `HotplugMonitor` trait with a
  fake queue and a read-only sysfs monitor (DRM by default, injectable
  `DisplayBackend` for Xrandr), `HotplugEvent` add/remove events,
  a quiet-window `HotplugDebounce` that cancels transient add+remove pairs, and
  a `SeatReconciler` that replans, stops disappeared seats, starts new seats,
  restarts changed plans, preserves backend labels, and reports typed action
  diagnostics. Operator exclusions can skip a seat, display, or input hub.
- **Opt-in per-seat audio**: `SeatAudioRouter` is the synchronous supervisor
  seam; `OzmaAudioRouter` adapts `ozma_audio::AudioRouter` without nesting
  runtimes. With a router installed, start ensures `ozma-seat-N` (and reports
  `ozma-seat-N.monitor`), and stop/remove/shutdown unloads it. A missing
  `pactl`/audio server is a typed `Audio` apply error in status, non-fatal to
  the seat; a failed removal leaves the applied flag for retry. Dry-run and the
  default (no-router) path run no audio command.
- Read-only controller seat grouping (`GET /api/v1/seats`, dashboard Seats
  panel) that recognises the `<machine>-seat-<n>` names the planner produces,
  or explicit `seat_id`/`parent_node_id` metadata, and reports online state,
  seat indexes, node ids, displays, and HID/HTTP ports. When a node serves its
  own read-only `GET /api/v1/seats`, the controller consumes the explicit
  persisted seat identity and plan/state metadata; older nodes and nodes without
  the route preserve the historical grouping. A node's typed seat-store error is
  passed through in `node_errors`; no node is dropped. It never merges nodes,
  controls seats, or exposes tokens or local store paths.
- **Read-only node seat report** (`ozma-node --multiseat --seats-file PATH`):
  opt-in and off by default. The node loads the versioned seat store once and
  serves the persisted plan plus per-seat status at public `GET /api/v1/seats`.
  `seat_id` is only the explicit `OZMA_SEAT_ID` value. The report marks a
  persisted seat whose display this node cannot observe as `available:false`
  with a typed `persisted_seat_missing_display` diagnostic; it never reports it
  running. A store read/parse/version failure is a typed 503 document. The route
  has no start/stop surface (`lifecycle:"external"`, `start_stop:false`) and
  spawns no process, including in plan-only use.

Deliberately not implemented yet:

- **Node-owned seat lifecycle.** `ozma-node` exposes only the read-only report
  above. Starting, restarting, and stopping seat children remains with the
  standalone `ozma-multiseat` supervisor; `ozma-softnode` has no seat
  integration. There is no manager API that lets a node or controller start a
  seat, and no process-group/namespace isolation for seat children.
- **True per-seat screen ownership.** The default X11 path positions one xrandr
  output on a single X server. All outputs share one root window and one
  keyboard focus, so `xrandr` ownership is not screen isolation. The opt-in
  `--seat-display xvfb` (and nested `xephyr`) backend now provides a private X
  root per seat, but it uses a virtual X server, not a physical output owner:
  xrandr output ownership is not applied on that root and is reported as a typed
  `ownership_unsupported` note. DRM leasing, one compositor per physical output,
  and Wayland compositor seats are still not implemented.
- **Real exclusive evdev grabs by default.** `EvdevGrabRouter` still models
  `EVIOCGRAB` with an unsupported default backend. The real path is the
  separate opt-in `ozma-seat-input` helper (`--input-isolation helper`);
  Xorg `InputDevice` sections are still not generated automatically. The
  operator supplies an Xorg config whose `InputDevice` identifiers equal the
  helper's `ozma-seat-...` names, as the live smoke does.
- **Automatic routing/ownership/audio/isolation execution is opt-in.** The CLI
  executes the planned ownership/routing/audio commands only with
  `--apply-ownership`, `--apply-routing`, and/or `--audio-router`, and the
  helper only with `--input-isolation helper`; all default off, and
  `--print-plan`/`--dry-run` never execute them. There is no policy engine,
  per-seat retry loop beyond the supervisor's restart path, or automatic
  reconciliation after external X layout changes. Evdev routing still requires
  an injected backend. With a private display, `xinput map-to-output` has no
  physical pointer to map and is reported as a typed
  `xinput_routing_unsupported` note instead of a fake success.
- **Compositor integration for private displays.** `Xvfb`/`Xephyr` prove real
  per-seat display isolation for X11 applications, and a real `Xorg` with dummy
  video + libinput proves per-display uinput enumeration and event routing, but
  no window manager, compositor, DRM scanout, physical-output handoff, or
  per-node compositor launch is managed. `xinput` targeting restricts a private
  display to the seat's helper uinput devices; `Xvfb` exposes only core/XTEST
  devices, so a live `Xvfb` targeting attempt honestly fails with
  `input_targeting_failed`, while the validated Xorg path passes. Kernel evdev
  ownership (`EVIOCGRAB`) remains with the helper, not a DRM-lease/compositor
  seat.
- **Kernel/compositor seat targeting for the helper's uinput devices.** The
  helper creates, names, and relays to per-seat uinput devices and the
  supervisor restricts a private X display to those names. There is still no
  node/compositor integration that claims a physical output for a seat or
  selects those devices as a compositor seat; the node's own uinput sink still
  does not choose an X screen.
- **Per-process audio routing and DSP.** `ozma-audio` creates/removes the null
  sink and exposes its monitor target; moving individual application streams,
  microphone remaps, room correction, and a Windows virtual audio endpoint are
  not wired into the multiseat supervisor.
- **Hotplug is a polling model, not udev/netlink.** Polling the opt-in
  `SysfsHotplugMonitor` compares a display snapshot and input-device snapshots;
  it debounces in the caller. The CLI injects an `XrandrDisplayBackend` so
  display names/screens match the initial plan; the library default uses DRM
  connectors (no X screen) for side-effect-free detection. There is no
  inotify/netlink subscription, no scale-up beyond discovered displays, and no
  automatic store write from the library (the CLI owns the file).
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
| DRM lease | `DrmLeaseBackend`, `RealDrmLeaseBackend`, `DrmCard`, `DrmCardSelector`, `DrmCrtcSelector`, `DrmLeaseRequest`, `DrmLeaseHandle`, `DrmLeaseObjects`, `DrmLeaseStatus`, `DrmLeaseNote`, `DrmLeaseError`, `enumerate_drm_cards`, `apply_lease_environment`, `clear_lease_environment` |
| Input | `InputDevice`, `InputDeviceKind`, `InputGroup`, `InputBackend`, `SysfsInputBackend`, `group_inputs`, `classify_capabilities`, `usb_hub_path_from_sysfs` |
| Host | `HostTopology` |
| Planning | `PlannerOptions`, `CapturePreference`, `MultiseatPlanner`, `PersistedPlan`, `allocate_ports` |
| Persistence | `SeatStore`, `PersistedSeat`, `PersistedSeatStatus`, `SeatStoreStatus`, `PersistedDisplay`, `SEAT_STORE_VERSION`, `DEFAULT_SEAT_STORE_NAME`, `default_seat_store_path` |
| Hotplug | `HotplugMonitor`, `HotplugEvent`, `HotplugDebounce`, `HotplugOptions`, `SeatReconciler`, `ReconcileReport`, `SysfsHotplugMonitor` |
| Audio | `SeatAudioRouter`, `SeatAudioSpec`, `SeatAudioState`, `OzmaAudioRouter` |
| Seat | `SeatProfile`, `ProfileDefaults`, `SeatConfig`, `SeatPlan`, `CapturePlan`, `NodeCommand`, `SeatPorts`, `SeatState`, `SeatStatus`, `SeatRuntimeStatus` |
| Input routing | `InputRouter`, `XInputRouter`, `EvdevGrabRouter`, `InputTarget`, `InputAssignment`, `InputRelease`, `InputRoutePlan`, `InputRoutingStrategy`, `assign_seats`, `parse_xinput_list`, `XInputDevice`, `RouteCommand`, `CommandRunner`, `ProcessCommandRunner`, `DeviceNameResolver`, `SysfsDeviceNameResolver`, `EvdevGrabBackend`, `EvdevGrabCommand`, `UnsupportedEvdevGrabBackend` |
| Input isolation | `SeatInputHelper`, `HelperClient`, `HelperClientConfig`, `InputIsolationHelper`, `HelperBackend`, `HelperDevice`, `HelperRequest`, `HelperResponse`, `HelperError`, `HelperErrorCode`, `HelperSeatStatus`, `InputIsolationAssignment`, `InputIsolationRelease`, `default_uinput_name`, `UINPUT_NAME_PREFIX` |
| Input targeting | `SeatInputTargeter`, `ProcessSeatInputTargeter`, `SeatInputTargetRequest`, `SeatInputTargeting`, `SeatInputTargetingRelease`, `InputTargetingMode`, `BoundedProcessCommandRunner`, `TARGETING_PROGRAM` |
| Display ownership | `DisplayOwnership`, `DisplayOwnershipKind`, `DisplayOwnershipController`, `XrandrOwnershipController`, `apply_ownership` |
| Seat display | `SeatDisplayBackend`, `SeatDisplayOptions`, `SeatDisplaySize`, `SeatDisplayRequest`, `SeatDisplayHandle`, `SeatDisplayLauncher`, `ProcessSeatDisplayLauncher`, `SeatDisplayPrograms`, `DisplayNumberAllocator`, `SeatDisplayStatus`, `SeatDisplayError`, `SeatDisplayNote`, `SeatDisplayNoteKind`, `probe_x11_display`, `display_server_args` |
| Plan result | `MultiseatPlan`, `PlanDiagnostic`, `DiagnosticCode` |
| Supervisor | `SeatSupervisor`, `ProcessSeatSupervisor`, `SupervisorOptions`, `NodeLaunchSpec`, `NodeLauncher`, `ReadinessProbe`, `SeatRuntimeStatus`, `SeatResourceStatus`, `SeatResourceError`, `ResourceKind`, `ResourceAction` |
| Status | `MultiseatCapability`, `MultiseatStatus`, `render_metrics` |
| Supervision | `SeatSupervisor`, `ProcessSeatSupervisor`, `SupervisorOptions`, `NodeLaunchSpec`, `NodeLauncher`, `ProcessLauncher`, `ChildHandle`, `ChildExit`, `ReadinessProbe`, `HttpHealthProbe`, `PortProbe`, `restart_delay` |
| CLI | `ozma-multiseat` binary |
| Test-only | `fake::FakeDisplayBackend`, `fake::FakeInputBackend`, `fake::FakeSeatSupervisor`, `fake::FakeNodeLauncher`, `fake::FakeReadinessProbe`, `fake::FakeCommandRunner`, `fake::FakeDeviceNameResolver`, `fake::FakeEvdevGrabBackend`, `fake::FakeSeatAudioRouter`, `fake::FakeSeatDisplayLauncher`, `fake::FakeSeatInputHelper`, `fake::FakeSeatInputTargeter`, `fake::FakeHotplugMonitor` |

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

When a [`SeatStore`] is supplied through
`MultiseatPlanner::plan_with_store` (or `HostTopology::plan_with_store`), a
discovered display matches a persisted seat by output name and reuses its id,
ports, audio sink, input hub, and profile even if display indexes were
reordered. A new display gets the first unused `<machine>-seat-<n>` and the
first unused UDP/HTTP pair at or above the configured bases. Persisted ids and
ports remain reserved while their display is missing, so a replug restores the
same identity. A persisted display or hub that is not discovered produces a
`PersistedSeatMissingDisplay`/`PersistedSeatMissingInput` diagnostic and is not
planned. Device paths missing from an otherwise present hub are also diagnosed.

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

The audio sink and `monitor:<sink>` capture target are **configuration intent**
in the plan. The default supervisor does not create the sink. With
`--audio-router` (or `ProcessSeatSupervisor::with_audio_router`) the audio layer
ensures the null sink before spawn and removes it after the child exits; a
missing helper/audio server is a typed, non-fatal status error. The node reports
a warned no-audio state if the sink does not exist.

## Input routing and display ownership

These layers turn the plan's device paths and output names into explicit
commands. They are **opt-in** in the CLI and supervisor: with the flags off (the
default) the plan is printed and injected into the child environment only; with
`--apply-ownership`/`--apply-routing` the supervisor calls the
`XrandrOwnershipController` and `XInputRouter` around the child lifecycle, and
with `--input-isolation helper` it asks the privileged helper for a confirmed
grab. A caller can still construct a router/controller directly.

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

### Real input isolation (`--input-isolation helper`)

The xinput strategy maps a shared pointer inside one X server. Real isolation
needs exclusive access to the physical evdev nodes. That is supplied by the
separate `ozma-seat-input` helper, which is the only component that opens
`/dev/input/event*`, issues `EVIOCGRAB`, or creates `/dev/uinput` devices.

- **Opt-in selection.** `--input-isolation xinput` is the default and keeps the
  existing xinput plan. `--input-isolation helper` rewrites each seat's
  `InputRoutePlan` strategy to `seat-input-helper`, makes `grab_exclusive` true,
  and installs `HelperClient`. It is mutually exclusive with `--apply-routing`,
  because a grabbed device cannot also be shared through xinput.
- **Helper protocol.** The supervisor spawns the helper (default program
  `ozma-seat-input`, override with `--input-helper PATH`) and speaks one JSON
  object per line over stdin/stdout. Requests are `hello`, `grab {seat_id,
  devices}`, `release {seat_id}`, `status`, and `shutdown`. Responses are
  `ready {protocol, helper, backend, pid}`, `grabbed {seat_id, grabbed,
  uinput_devices}`, `released`, `status`, `shutting_down`, or a typed
  `error {code, message, seat_id, device}`. The client checks the protocol
  version and refuses a `fake` backend unless a test explicitly opts in.
- **Validation and ownership.** The helper accepts only lexical
  `/dev/input/event<digits>` paths, rejects duplicates in one request, and
  rejects a device already grabbed by another seat or a changed set for a seat
  that is already grabbed. It validates the whole request before touching a
  device. The real backend additionally rejects symlinked event paths.
- **Ordering and rollback.** For each seat the helper grabs each device, creates
  one uinput device mirroring the source's key/relative/absolute/switch
  capabilities, and starts a relay thread that forwards events. Uinput names are
  `ozma-seat-<seat>-<kbd|mouse|pad|other>-<eventN>`, so a display targeter can
  recognise a seat without parsing a hyphenated seat id. If any step fails, it
  stops relays, destroys uinput devices, and ungrabs every device already
  acquired before returning the typed error. No partial assignment is recorded.
- **Private-display targeting.** A helper grab alone does not stop another
  seat's uinput devices from appearing on a private X server. After display
  readiness and the confirmed grab, `ProcessSeatInputTargeter` runs `xinput list
  --short` with `DISPLAY` and, when present, `XAUTHORITY` for the seat, then:
  - validates that every helper-confirmed own name is visible before changing
    anything; a missing own device is a typed `InputTargeting` error;
  - disables every other `ozma-seat-...` device (other seats' devices), with
    rollback re-enabling them if a later command fails;
  - enables the seat's own names. Core pointer/keyboard/XTEST devices and
    non-`ozma-seat-...` devices are never enabled, disabled, or detached.
  Each `xinput` call is bounded (5 s by default). `--input-targeting warn`
  (default) records the typed `input_targeting` apply error and
  `input_targeting_failed` display note but still starts the seat; `strict`
  returns the error, releases the helper grab and private display, and never
  spawns; `off` skips targeting with an `input_targeting_unavailable` note.
  Targeting is never reported applied when it failed.
- **Supervisor semantics.** With `input_isolation` enabled, start applies
  ownership (if requested), then the confirmed helper grab, then private-display
  targeting, then audio; on any failure the seat is not spawned, the typed
  `input_isolation`/`input_targeting` apply error is stored, and earlier seats
  are released. Stop/remove/rollback/`Drop` release targeting (re-enabling other
  seats' devices) before isolation, routing, and ownership. A release failure
  leaves its applied flag set for a later retry and is kept in status. The
  private display is stopped last, and its teardown clears targeting state
  without running commands against a dead server.
- **Crash/restart honesty.** `tick` re-checks each applied seat with the helper.
  If the helper restarted or lost the grab, the applied flag and grabbed count
  are cleared, targeting is cleared, a typed error is recorded, and a fresh grab
  and targeting apply are attempted. A private-display crash clears targeting
  before the replacement server starts, so the new display is re-targeted. If
  the fresh grab/targeting fails, an unisolated running child is terminated and
  scheduled for restart instead of leaving a false isolation claim.
- **SIGTERM restore.** The helper blocks SIGTERM/SIGINT in the main thread,
  waits on a safe `sigwait` thread, releases every seat's relays/uinput devices,
  and exits. EOF or a malformed client also triggers release/shutdown.
- **Status.** Each seat's `resources` carries `input_isolation_applied`,
  `input_isolation_released`, `input_targeting_applied`,
  `input_targeting_released`, `grabbed_device_count`, `uinput_devices`
  (for example `ozma-seat-host-seat-0-kbd-event0`), `targeted_devices` (own
  names enabled on the private display), `disabled_devices` (other-seat names
  disabled there), plus typed `{"kind":"input_isolation"|"input_targeting",...}`
  errors. Nothing is redacted: the status is loopback-only and is meant to be
  diagnostic.
- **Default behavior.** With the flag absent, the helper is not spawned, no
  device is opened, and the modelled `EvdevGrabRouter` still returns a typed
  `Unsupported` error if an evdev strategy is requested without a real backend.

```text
{"op":"hello","protocol":1}
{"event":"ready","protocol":1,"helper":"ozma-seat-input","backend":"real","pid":4242}
{"op":"grab","seat_id":"host-seat-0","devices":[{"path":"/dev/input/event3","kind":"keyboard"}]}
{"event":"grabbed","seat_id":"host-seat-0","grabbed":["/dev/input/event3"],"uinput_devices":["ozma-seat-host-seat-0-kbd-event3"]}
{"op":"release","seat_id":"host-seat-0"}
{"event":"released","seat_id":"host-seat-0","released":["/dev/input/event3"]}
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

**Single X server limitation.** On the default backend, all xrandr outputs
share one root window, one window manager, and one keyboard focus. Positioning
an output and mapping a pointer to it is not seat isolation; `ozma-node` still
injects into the one X server. The opt-in `--seat-display xvfb|xephyr` backend
below starts a private X server per seat, which does isolate the root/focus for
X11 applications. It is still not DRM leasing or a physical-output/compositor
seat, and it claims no Wayland or Windows ownership.

## Per-seat X servers (`--seat-display`)

The display backend is selected per supervisor run, not per seat:

| Backend | Behavior |
|---|---|
| `shared` (default) | No server is started. Every seat uses the host's one X root; `DISPLAY` comes from the plan. Existing behavior is unchanged. |
| `xvfb` | Start `Xvfb :N -screen 0 WxHx24 -nolisten tcp` per seat, on a free display number. |
| `xephyr` | Start `Xephyr :N -screen WxHx24` with `DISPLAY` set to the shared display, so the private X root is a window inside the operator's desktop. If `Xephyr` is unavailable the start fails with a typed `Unsupported` error. |
| `xorg` | Start a real `Xorg :N -config <file> -logfile <file> -nolisten tcp -noreset -novtswitch -sharevts -keeptty -auth <Xauthority>` per seat. Geometry, dummy/video, and libinput devices come from the config. Without `xorg_config` the start is a typed `Unsupported` error instead of a host GPU probe. |

`SeatDisplayOptions` carries `backend`, `size` (default `1280x800`), the display
number range (default `90..=199`), readiness timeout/poll, an optional shared
parent for Xephyr, and `xorg_config` for the Xorg backend. `SeatDisplaySize` is
command-line geometry for Xvfb/Xephyr; Xorg takes geometry from its config.
`ProcessSeatDisplayLauncher` is the production
spawner; it is installed automatically by `ProcessSeatSupervisor::new` when the
backend is nested. `with_parts` (used by tests and the CLI) requires an explicit
`with_display_launcher(...)`.

Lifecycle and honesty rules:

- **Allocation.** The allocator walks the configured range and skips numbers
  whose `/tmp/.X11-unix/XN` socket or `/tmp/.XN-lock` file already exists. A
  released number is reused by the next start. Exhaustion is
  `SeatDisplayNumberExhausted` (`number_exhausted` in status).
- **Auth.** When `xauth` is available, the launcher creates a private temporary
  `Xauthority` with a random `MIT-MAGIC-COOKIE-1` for `:N`, starts the server
  with `-auth <file>`, and exports `XAUTHORITY` to the child. If `xauth` is
  missing or fails, the server uses `-ac` and a typed `auth_disabled` note is
  recorded; the listener is still Unix-socket-only because `-nolisten tcp` is
  always passed.
- **Readiness.** A spawned process is not a ready display. The supervisor
  completes an X11 connection setup handshake against `/tmp/.X11-unix/XN`
  (with the cookie) and only then stores the handle and reports
  `display_ready=true`. A server that exits first is `SeatDisplayCrashed`; one
  that never answers is `SeatDisplayStartupTimeout`. Both terminate and release
  the number before returning.
- **Child environment.** `DISPLAY` and (when present) `XAUTHORITY` are exported.
  X11 capture flags are rewritten to the private display at offset `0,0` with
  the configured `--seat-display-size`. A PipeWire capture seat keeps its
  configured target; only the application display changes.
- **Start order.** Private X server → ownership/routing/helper/audio resources →
  private-display input targeting → node child. **Stop order** is node child →
  targeting/resource release → private X server. A failed resource apply or node
  readiness releases the server, and rollback releases servers that have no
  child.
- **Crash.** `tick` polls each private server. If it exits while the seat is
  running, the node is terminated, targeting state is cleared, the server is
  released, and the seat is scheduled for a bounded restart. The next attempt
  starts a fresh server, reuses the display number, and re-applies targeting.
- **Ownership/routing on a private root.** xrandr output ownership and xinput
  `map-to-output` assume a physical output/pointer in the shared X server.
  With a nested display they are **not executed**; the seat reports typed
  `ownership_unsupported` / `xinput_routing_unsupported` notes instead of a
  fake applied flag. `--input-isolation helper` still runs before the node and
  is not regressed: the helper grabs the devices, creates the per-seat uinput
  devices, and `--input-targeting warn|strict` restricts the private display to
  those names with `xinput`. A display that does not expose those uinput nodes
  (for example `Xvfb`) reports `input_targeting_failed` rather than claiming
  isolation; a real Xorg/libinput display exposes them and passes. On the live
  Xorg smoke, disabling seat B's devices on `:90` and seat A's on `:91` also
  stopped injected events from crossing displays.
- **Status.** Each seat carries `display_backend`, `display`, `display_pid`,
  `display_ready`, `display_notes`, and a typed `display_error`
  (`missing_binary`, `number_exhausted`, `startup_timeout`, `crashed`,
  `unsupported`). All display fields are omitted for the default `shared`
  backend, so existing status JSON is unchanged.

## DRM leases (`--seat-drm-lease`)

This is an opt-in **lease-only** handoff, not a new display backend that draws
on the leased output. A seat can receive an owned lessee fd for a named
connector and CRTC; nothing in this crate programs scanout, starts a
compositor, or claims that the host GPU/desktop is isolated. The default
display backend remains `shared`.

Module: `ozma-multiseat/src/drm_lease.rs`.

| Type | Purpose |
|---|---|
| `DrmCard` / `DrmLeaseConnector` | Sysfs card name/path/driver/connectors and, after `inspect`, CRTC and connector object ids. |
| `DrmCardSelector` | `auto`/`vkms` (virtual only) or an explicit `/dev/dri/cardN`. |
| `DrmCrtcSelector` | `auto` or an explicit object id. |
| `DrmLeaseRequest` | `[SEAT=]CARD:CONNECTOR[@CRTC]`, parsed by `FromStr`. |
| `DrmLeaseBackend` | `enumerate`, `inspect`, `acquire`, `list_lease_ids`; production implementation is `RealDrmLeaseBackend`. |
| `DrmLeaseHandle` | Owned `fd`, lessee/object ids, `validate` (`GET_LEASE`), `list_lease_ids`, `revoke`, `is_active`, `set_inheritable`. |
| `DrmLeaseStatus` | Flattened seat status: card/connector, lessee id, connector/CRTC/plane ids, exported fd, applied/released flags, typed notes and error. |

### Safety and object validation

- `AutoVirtual` only selects a card whose sysfs driver is `vkms` (including the
  newer `faux_driver` binding used by vkms). Real GPUs are never selected
  implicitly; the operator must name `/dev/dri/cardN` (or `cardN`).
- The connector is resolved from the card's current resource handles and its
  `Info` name, never from a hard-coded id. An explicit CRTC id is checked
  against `resource_handles().crtcs()`. Unknown objects are
  `DrmLeaseError::UnknownObject`.
- A same-process duplicate is rejected by an object registry before any ioctl;
  the kernel remains authoritative for cross-process conflicts and returns a
  typed `AlreadyLeased`/`LeaseFailed` error. No fake lease is ever returned.
- Setting DRM master (`SET_MASTER`) is attempted automatically only for a
  virtual `vkms` card. A real card usually has a compositor as master; ozma
  does not steal it. Without master/CAP_SYS_ADMIN the result is
  `CapabilityRequired`, not a lease.
- The lessee fd is owned by the handle. `revoke()` issues
  `DRM_IOCTL_MODE_REVOKE_LEASE` and closes it; dropping the handle closes the
  fd, which also releases the kernel lease. `GET_LEASE` validates the object
  set, and `list_lease_ids` uses the lessor's master fd.

### Typed errors

| Error | `code()` |
|---|---|
| `NoDrm` | `no_drm` |
| `NoPermission` (open denied) | `no_permission` |
| `Unsupported` (driver/kernel/ioctl) | `unsupported_lease` |
| `ExplicitCardRequired` (auto chose a non-virtual card) | `explicit_card_required` |
| `UnknownObject` / `UnknownConnector` | `unknown_object` |
| `AlreadyLeased` | `already_leased` |
| `CapabilityRequired` (master/CAP_SYS_ADMIN) | `capability_required` |
| `UnknownCard`, `InvalidRequest`, `Io`, `LeaseFailed`, `Released` | matching snake-case names |

### Supervisor lifecycle

`SupervisorOptions::drm_leases` maps seat id to `DrmLeaseRequest`. The CLI's
repeatable `--seat-drm-lease` populates it. `ProcessSeatSupervisor::new`
installs the production backend when at least one request exists; tests/the CLI
using `with_parts` call `with_drm_lease_backend(...)` explicitly. Without a
backend, a requested lease fails with a typed `Unsupported` error before the
node spawns.

- **Start order:** private display (if any) → DRM lease acquire → ownership/
  routing/helper/audio resources → node child. A lease failure releases the
  display and earlier seat resources and never spawns the node.
- **Child export:** the supervisor clears `FD_CLOEXEC` only around the spawn,
  then restores it. The child receives `OZMA_DRM_LEASE=1`, `OZMA_DRM_FD`,
  `OZMA_DRM_LEASE_ID`, `OZMA_DRM_CARD`, `OZMA_DRM_CONNECTOR`,
  `OZMA_DRM_CONNECTOR_ID`, and `OZMA_DRM_CRTC_ID`. No unknown argv is appended.
- **Restart:** an active lease is reused and its environment re-applied; a
  released lease is re-acquired before the restart spawn.
- **Stop/remove/rollback/Drop:** the node is stopped first, then the lease is
  revoked, then display-dependent resources/display are released. A revoke
  failure keeps the handle and `drm_lease_applied` for a later retry.
- **Dry run:** reports `would acquire a DRM lease ... (lease-only; no scanout or
  compositor isolation)` and acquires nothing.
- **Status:** `drm_lease` (omitted when no lease was requested) carries the
  object ids, lessee id, fd, `applied`/`released`, a typed error, and the
  `lease_only_no_scanout` note. `resources.drm_lease_applied` /
  `drm_lease_released` mirror the lifecycle.

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
  `Unsupported` error. With `input_isolation`, routing is skipped and the
  helper grabs the seat's complete device set after ownership. A failed apply
  never spawns that seat, records a typed `SeatResourceError`, and releases
  every resource already applied for earlier seats in reverse order.
  Controllers are installed with `with_ownership_controller`,
  `with_routing_controller`, `with_evdev_routing_controller`,
  `with_input_isolation_helper`, and `with_input_targeter`; without the flags no
  controller/helper/targeter is called.
- **Input isolation (opt-in).** `SeatInputHelper` is the supervisor seam;
  `HelperClient` starts the privileged process lazily, negotiates protocol
  version 1, and reports only helper-confirmed grabs. `resources` carries the
  grabbed count, uinput names, and typed errors. A helper crash clears the
  applied claim on the next tick; targeting is cleared, a re-grab and
  re-targeting are attempted, and a running child is stopped/restarted when
  isolation cannot be restored. Dry-run never starts the helper. When the last
  confirmed seat is released, the helper is asked to release everything and
  stop; the next start starts it lazily again.
- **Input targeting (opt-in with private displays).** `SeatInputTargeter` is the
  display-content seam. `ProcessSeatInputTargeter` runs the real `xinput`
  commands, while `FakeSeatInputTargeter` records requests and failures in
  tests. Targeting applies after display readiness and the helper grab, before
  the child. Stop/rollback/release re-enables the other seats' devices while the
  private display still exists. `InputTargetingMode` is `Warn` by default;
  `Strict` aborts the seat, `Off` skips it. The shared display backend is always
  a targeting no-op.
- **Private displays (opt-in).** With `seat_display.backend = Xvfb`, `Xephyr`,
  or `Xorg`, start spawns and handshakes the server before resource reservation
  and rewrites the launch spec. Stop releases resources and then stops the server.
  A failed display start never spawns a node; a display crash stops the node and
  restarts both. Dry-run reports the would-be server without allocating or
  spawning it. Tests inject `FakeSeatDisplayLauncher`.
- **Audio (opt-in).** With a `SeatAudioRouter` installed, start ensures the
  seat's `ozma-seat-N` null sink and records the `<sink>.monitor` target before
  spawning; stop/remove/rollback/shutdown unload it. An ensure failure is
  recorded as a typed `Audio` apply error and does **not** prevent the seat from
  starting; a remove failure leaves `audio_applied=true` and retains the seat for
  a later `remove` retry. Dry-run never calls the router.
- **DRM leases (opt-in, lease-only).** With `SupervisorOptions::drm_leases`, start
  acquires the seat's connector/CRTC lease after any private display and before
  resource reservation, then exports the lessee fd through `OZMA_DRM_FD`. The
  lease is revoked on stop/remove/rollback/Drop; a failure prevents the spawn
  and releases earlier seats. Restarts reuse an active lease. This never starts
  a compositor or programs scanout; status always carries the
  `lease_only_no_scanout` note.
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
  routing/ownership/audio controller even when the flags are set.
- **Persistence and hotplug.** `SeatStore::save_atomic` writes `seats.json`
  through a same-directory temporary file and rename. `SeatReconciler` takes a
  planner, store, current topology, and a `HotplugMonitor`; it replans, stops
  seats whose displays disappeared, starts new displays, and restarts a seat
  whose plan changed. `HotplugDebounce` waits for a quiet window and cancels a
  transient add+remove pair. `HotplugOptions` excludes seat ids, displays, and
  hubs from action.
- **Tests.** `ProcessSeatSupervisor::with_parts` injects a `NodeLauncher` and a
  `ReadinessProbe`. `FakeNodeLauncher` records spawns, simulates exits and
  spawn failures, and records stop requests; `FakeReadinessProbe` can be ready,
  not-ready, delayed, or failing. `FakeCommandRunner` plus
  `FakeDeviceNameResolver` drive the real `XrandrOwnershipController` and
  `XInputRouter` without running `xrandr`/`xinput`. `FakeSeatDisplayLauncher`
  simulates ready/timeout/crash/number-exhaustion/missing-binary display starts
  without opening a socket. `FakeSeatInputHelper` and `FakeSeatInputTargeter`
  simulate confirmed grabs, targeting failures, releases, and clears without a
  helper process or X server.

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
      "drm_lease": {
        "card": "/dev/dri/card3",
        "connector": "Virtual-1",
        "lessee_id": 1,
        "connector_id": 43,
        "crtc_id": 41,
        "plane_ids": [],
        "fd": 12,
        "applied": true,
        "released": false,
        "notes": [
          {
            "kind": "lease_only_no_scanout",
            "message": "DRM lease grants only the named connector/CRTC object IDs ..."
          }
        ]
      },
      "resources": {
        "ownership_applied": true,
        "routing_applied": false,
        "input_isolation_applied": true,
        "input_targeting_applied": true,
        "audio_applied": true,
        "drm_lease_applied": true,
        "ownership_released": false,
        "routing_released": false,
        "input_isolation_released": false,
        "input_targeting_released": false,
        "audio_released": false,
        "grabbed_device_count": 2,
        "uinput_devices": [
          "ozma-seat-host-seat-0-kbd-event0",
          "ozma-seat-host-seat-0-mouse-event1"
        ],
        "targeted_devices": [
          "ozma-seat-host-seat-0-kbd-event0",
          "ozma-seat-host-seat-0-mouse-event1"
        ],
        "disabled_devices": [
          "ozma-seat-host-seat-1-kbd-event2",
          "ozma-seat-host-seat-1-mouse-event3"
        ]
      },
      "display_backend": "xvfb",
      "display": ":90",
      "display_pid": 4240,
      "display_ready": true,
      "display_notes": [
        {
          "kind": "ownership_unsupported",
          "message": "xrandr ownership of output HDMI-1 is not applied: ..."
        }
      ]
    }
  ],
  "diagnostics": []
}
```

A failed seat also carries `last_error` and, while a restart is pending,
`next_restart_in_ms`. Resource apply/release failures add entries such as
`{"kind":"ownership","action":"release","message":"..."}`,
`{"kind":"input_isolation","action":"apply","message":"helper error [grab_failed]: ..."}`,
`{"kind":"input_targeting","action":"apply","message":"seat input targeting error: ..."}`
or `{"kind":"audio","action":"apply","message":"pactl is not installed"}` to
`resources.errors`; the field is omitted entirely when no resource action
happened. Private display failures add
`{"kind":"startup_timeout"|"crashed"|"missing_binary"|"number_exhausted"|"unsupported","message":"..."}`
as `display_error`. Targeted private displays add
`input_targeting_failed` / `input_targeting_unavailable` notes when targeting
did not fully apply. All private display fields are omitted when
`display_backend` is `shared`. A DRM lease adds `drm_lease` (object ids, lessee
id, fd, applied/released, typed error and the `lease_only_no_scanout` note) and
`resources.drm_lease_applied`/`drm_lease_released`; both are omitted when no
lease was requested. `capability.node_count` always equals
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

## Node read-only seat report (`ozma-node --multiseat`)

Seat lifecycle stays with the `ozma-multiseat` supervisor. A node launched by
that supervisor can additionally expose the persisted plan it came from:

```bash
ozma-node --profile desktop --name host-seat-0 \
  --multiseat --seats-file /etc/ozma/seats.json
```

`--multiseat` (visible alias `--multiseat-plan`) is opt-in and requires
`--seats-file`. Without the flag the public route is present but reports
`configured:false`, so a controller can distinguish "no multiseat" from an
error. The store is loaded once at startup; the node does not enumerate
displays, run `xrandr`, or re-plan seats.

`GET /api/v1/seats` returns:

```json
{
  "ok": true,
  "configured": true,
  "mode": "read-only",
  "lifecycle": "external",
  "start_stop": false,
  "source": "/etc/ozma/seats.json",
  "seat_id": "host-seat-0",
  "plan": {"version": 1, "seats": [{"id": "host-seat-0", "...": "..."}]},
  "status": {
    "version": 1,
    "seat_count": 1,
    "node_count": 1,
    "seats": [
      {
        "seat_id": "host-seat-0",
        "display_output": "HDMI-1",
        "state": "planned",
        "available": true,
        "...": "persisted ports/profile/input fields"
      }
    ],
    "diagnostics": []
  },
  "node_seat": {"seat_id": "host-seat-0", "state": "planned", "available": true}
}
```

* `seat_id` is the explicit `OZMA_SEAT_ID` environment value only; the node
  never derives one from `--name`. `node_seat` is present only when that id
  names a persisted seat.
* Runtime lifecycle fields (`pid`, `restarts`, resource state) are absent: the
  node does not own the seat process. `lifecycle` is always `"external"` and
  `start_stop` is always `false`.
* `available:false` plus a `persisted_seat_missing_display` diagnostic means the
  node did not observe that persisted output (from configured display outputs or
  the explicit `OZMA_X11_OUTPUT`). The seat stays retained and `planned`; it is
  never resurrected as running.
* An unreadable, malformed, newer-version, or invalid store is a typed 503:
  `{"ok":false,"configured":true,"mode":"error","error":{"code":"seat_store_parse","message":"..."}}`.
  It contains no fabricated seats. Codes are stable (`seat_store_read`,
  `seat_store_parse`, `seat_store_version`, `seat_store_invalid`,
  `seat_store_conflict`).

This is the complete node-owned surface. There is no `POST /api/v1/seats`,
start, stop, or restart route; the supervisor remains the only component that
mutates seat lifecycle.

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
- Each node is asked for its own read-only `GET /api/v1/seats` (bounded timeout,
  bearer token when configured). A `configured:true` report supplies the
  explicit persisted `seat_id` and a bounded `multiseat` object
  (`mode`, `plan_seat_count`, `state`, `available`) on the seat entry. This
  lets a renamed node still join its machine group. The node's local store path,
  plan, argv, and environment are never passed through.
- A node that does not serve the route (404), has no HTTP API, or answered
  `configured:false` keeps the historical name/metadata grouping exactly.
- A reachable node's typed failure (for example `seat_store_parse`) is preserved
  in a top-level `node_errors` list as `{node_id, status, error:{code,message}}`;
  the node still appears through the fallback grouping. `node_errors` is omitted
  when every node is absent/healthy, preserving the earlier response shape.
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

# Opt in to real per-seat EVIOCGRAB/uinput isolation. This needs the
# privileged helper and is mutually exclusive with --apply-routing.
ozma-multiseat --display :0 --input-isolation helper \
  --input-helper /usr/lib/ozma/ozma-seat-input

# Give each seat its own private Xvfb display at 1280x800. The servers are
# stopped with their seats; a crash restarts both. Xephyr is the nested-window
# alternative (`--seat-display xephyr`).
ozma-multiseat --display :0 --seat-display xvfb --seat-display-size 1280x800

# Opt in to a lease-only DRM handoff for one seat. `auto` selects only a vkms
# card; name an explicit /dev/dri/cardN for a real output. The fd is exported in
# OZMA_DRM_FD; this does not create scanout or compositor isolation.
ozma-multiseat --display :0 \
  --seat-drm-lease auto:Virtual-1

# Repeat once per seat, or use an explicit seat prefix/name.
ozma-multiseat --display :0 \
  --seat-drm-lease host-seat-0=/dev/dri/card3:Virtual-1

# Persist stable seat ids/ports/sinks across replans, reconcile sysfs hotplug,
# and create/remove per-seat null sinks through PipeWire's pactl API.
ozma-multiseat --display :0 \
  --seats-file ~/.config/ozma/seats.json \
  --hotplug --audio-router pipewire
```

A node launched by that supervisor can report the same persisted plan without
owning lifecycle:

```bash
# Read-only GET /api/v1/seats. Requires the store; never starts/stops a seat.
ozma-node --profile desktop --name host-seat-0 \
  --multiseat --seats-file /etc/ozma/seats.json
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
| `--input-isolation xinput\|helper` | Input path. `xinput` is the default; `helper` uses privileged `EVIOCGRAB`/uinput isolation. |
| `--input-helper PATH` | Helper binary for `--input-isolation helper` (default `ozma-seat-input`). |
| `--input-targeting off\|warn\|strict` | Restrict a private display to the seat's helper uinput devices. `warn` is the default; `strict` prevents the spawn on failure; `off` skips `xinput`. |
| `--seat-display shared\|xvfb\|xephyr` | Per-seat display backend. `shared` is the default; `xvfb`/`xephyr` start and stop a private X server per seat. |
| `--seat-display-size WxH` | Private X screen geometry (default `1280x800`). |
| `--seat-drm-lease [SEAT=]CARD:CONNECTOR[@CRTC]` | Opt-in lease-only handoff. Repeat once per seat, or prefix every value with a seat id/index. `auto`/`vkms` selects only a virtual vkms card; real outputs must be named explicitly. |
| `--seats-file PATH` | Load/reuse and atomically update persisted seat identities. Never written by `--print-plan`/`--dry-run`. |
| `--hotplug` | Poll read-only sysfs DRM/input snapshots, debounce, and start/stop/restart seats. |
| `--audio-router [pipewire\|pulseaudio]` | Ensure/remove `ozma-seat-N` null sinks around the child lifecycle (default kind `pipewire`). |
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
routing/ownership/audio after each child exits. Output lists applied/released
state and any release error; JSON carries `resources` on each seat. With the
flags off (the default) the CLI never runs `xrandr`, `xinput`, or `pactl` for
these resources, and `--print-plan`/`--dry-run` never run them even when the
flags are present.

With `--input-isolation helper`, start instead asks the helper to grab the
seat's complete device set. The helper must confirm every grab and uinput
device before the child is spawned. A missing helper, helper error, partial
confirmation, or cross-seat conflict is a typed start failure; earlier seats are
released. On stop/remove/shutdown the helper releases the seat. The local
helper result is visible in `resources.grabbed_device_count` and
`resources.uinput_devices`. The default `xinput` path does not start the helper
or open a device.

With a private `--seat-display xvfb|xephyr` and `--input-isolation helper`,
`--input-targeting` restricts that display after the helper grab and before the
node: the seat's own `ozma-seat-...` uinput devices are enabled, other seats'
helper devices are disabled, and core/XTEST devices are left alone. `warn`
(default) records a typed `input_targeting` error and continues, `strict`
refuses to spawn and releases the helper/display, and `off` skips `xinput`.
`resources.input_targeting_applied`, `targeted_devices`, and
`disabled_devices` only claim what the real `xinput` commands confirmed.
Targeting is never applied on the default shared display.

With `--seat-display xvfb` or `xephyr`, the supervisor starts a private X
server before each seat. A missing `Xvfb` (or `Xephyr` for the nested backend) is
a typed `display_error`; a server that never completes the X11 handshake is a
typed `startup_timeout` and is never claimed; an exhausted `:90..=199` range is
`number_exhausted`; a server that exits is `crashed`. Display numbers are
reused after stop or rollback. Output lists `display(<backend> <display>
pid=... ready=...)`; JSON carries the same fields. On shutdown all child
processes are stopped before their servers, and the sockets/processes are gone
when `stop_all` returns.

`--seat-drm-lease [SEAT=]CARD:CONNECTOR[@CRTC]` adds a lease-only handoff before
the node spawn. Without a seat prefix the flag must be repeated exactly once per
planned seat (in plan order); with prefixes, every value must be explicit.
`auto`/`vkms` selects only a virtual vkms card, while a real card must be named
as `/dev/dri/cardN`/`cardN`. The node receives the inherited fd and object ids
in `OZMA_DRM_*`; output lists `drm_lease(...)` and JSON carries `drm_lease` plus
`resources.drm_lease_*`. The CLI fails the start (and rolls back earlier seats)
when the lease cannot be created; it never substitutes a fake lease. This does
not add per-seat scanout or compositor isolation, and `--print-plan` never
acquires a lease.

`--seats-file` loads a versioned JSON store (a missing file is empty). The CLI
reuses persisted ids/ports/hubs/sinks for discovered displays, assigns unused
identities to new displays, prints missing-display/input diagnostics, and saves
the updated store atomically after a successful replan. A corrupt file, a newer
store version, or duplicate id/display/port is a typed startup error. Missing
displays are retained in the file but are never started. The file is not read
for side effects beyond loading and is not written by either no-spawn mode.

`--hotplug` adds the opt-in `SysfsHotplugMonitor`: it snapshots the explicit
Xrandr display (the CLI injects `XrandrDisplayBackend`) and classified input
devices, debounces a quiet window, and asks the `SeatReconciler` to
start/stop/restart seats. The monitor is polling and read-only; it does not
subscribe to udev/netlink, and the library's DRM-only default does not claim an
X screen for a connector. A display removal stops its seat, an addition
gets the next deterministic id/port, a hub add/remove restarts the seat with the
new input group (or a missing-input diagnostic), and a transient add+remove
inside one debounce window is a no-op. The store is updated by the CLI when a
reconciliation changes it.

`--audio-router` installs `OzmaAudioRouter` around the real `ozma-audio` router.
Start ensures the planned `ozma-seat-N` sink and its `<sink>.monitor` target
before the node spawns; stop/remove/shutdown unloads it. A missing `pactl`, an
unavailable audio server, or a non-null sink removal refusal is a typed `Audio`
entry in `resources.errors`; an ensure failure does not prevent the seat from
starting, and a remove failure leaves the applied flag so `remove` can retry.

The optional status listener is bound to `127.0.0.1` and exposes only
`/health`, `/status`, `/metrics`, and `/api/v1/connection`. It carries no
bearer tokens, node argv, environment, or operator secret.

## Packaging and systemd

`scripts/install-rust.sh` installs `ozma-multiseat` with the node binaries:

```bash
scripts/install-rust.sh --build --prefix "$HOME/.local"        # user layout
scripts/install-rust.sh --from-target target/release --prefix /tmp/ozma-review
scripts/install-rust.sh --dry-run --build                      # prints, writes nothing
scripts/install-rust.sh --build --no-multiseat                 # skip the binary/unit/env
```

The bin name is read from `ozma-multiseat/Cargo.toml` (no `[[bin]]` there, so it
resolves to the package name). When the binary is present in the target
directory (or built), the same run also installs:

```text
<bin-dir>/ozma-multiseat
<units-dir>/ozma-multiseat.service
<env-dir>/ozma-multiseat.env.example
```

`<bin-dir>` is `~/.local/bin` or `/usr/local/bin`, `<units-dir>` is
`~/.local/lib/systemd/user` or `/usr/local/lib/systemd/system`, and `<env-dir>`
is `~/.local/etc/ozma` or `/etc/ozma`. `--no-multiseat` skips all three; a
missing binary skips them with a note rather than failing the node install.
`scripts/install-system.sh` renders the same unit and env example (it never
installs binaries), including the operator enable/rollback commands under the
`desktop` profile.

The unit is a single-instance supervisor, not a template:

- `ExecStart=<bin>/ozma-multiseat --node-bin <bin>/ozma-node --seats-file
  @STATE_DIR@/seats.json --hotplug $OZMA_MULTISEAT_ARGS`. The persisted store is
  the same `seats.json` a seat node can read with `--multiseat --seats-file`.
- `--print-plan` and `--dry-run` are deliberately **not** used: both print and
  exit, which would make systemd restart-loop the unit.
- `Restart=on-failure`, `RestartSec=5s`, `StartLimitIntervalSec=300`,
  `StartLimitBurst=5`, `TimeoutStopSec=30s`, and `KillMode=mixed`: the
  supervisor gets SIGTERM and stops/reaps every seat child; leftover processes
  are killed after the timeout. Per-child restarts keep the CLI's own bounded
  backoff.
- `User=@OZMA_USER@`, `Group=@OZMA_GROUP@`,
  `SupplementaryGroups=video input render` match the node units. The user-unit
  rendering strips those system-only directives and retargets
  `WantedBy=default.target`.
- `PrivateTmp=no` because X11 sockets live in `/tmp/.X11-unix`;
  `ProtectSystem=strict`, `ProtectHome=yes`, `NoNewPrivileges=yes`, and the
  device allow-list mirror the desktop node.
- `DISPLAY` (and, for a system unit, `XAUTHORITY`) comes from
  `ozma-multiseat.env`. A system unit cannot read a user's `~/.Xauthority` under
  `ProtectHome=yes`; prefer `systemctl --user` for a live session or provide a
  readable host cookie path. Ownership/routing/audio/private-display/input
  isolation stay opt-in through `OZMA_MULTISEAT_ARGS`.

## Meeting detection packaging

`ozma-meeting-detect` (the `[[bin]]` selected from `ozma-softnode/Cargo.toml` by
its `meeting` name/path) is installed by `install-rust.sh` when built and can be
skipped with `--no-meeting-detect`. It has **no unit**: its only mode is a
foreground IPC server bound to `$XDG_RUNTIME_DIR/ozma-meeting-detect.sock`
(fallback `/tmp/ozma-meeting-detect.sock`), with no socket-path flag, no
system-service mode, and no controller report wiring yet. Run it on demand in the
session that owns the calendar data:

```bash
ozma-meeting-detect --interval 5 --ics-path ~/calendar.ics
```

`scripts/systemd/verify-units.sh` covers `ozma-multiseat.service` in both the
system and user renderings; `scripts/verify-system-install.sh` checks the
persisted-store command, the absence of `--print-plan`/`--dry-run` in
`ExecStart`, the account/restart settings, the env example, file modes, and the
operator enable/rollback lines.

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
- Input isolation (`tests/input_isolation.rs`): helper protocol JSON round-trip,
  `hello`/`grabbed`/`released`/`status`/`shutdown` handling, strict
  `/dev/input/eventN` validation, duplicate and cross-seat rejection, idempotent
  same-seat grab and release, grab/uinput/release failure rollback, and a real
  subprocess protocol check against `ozma-seat-input --backend fake`.
- Input-isolation supervisor integration (`tests/input_isolation_supervisor.rs`):
  confirmed grab before spawn, grabbed-count/uinput status, stop release, grab
  failure without spawn plus earlier-seat rollback, helper readiness failure,
  release failure retention and retry, helper crash detection and re-grab
  without respawning the node, cross-seat conflict failure, dry-run no-helper,
  and default isolation off.
- Input targeting (`tests/input_targeting.rs`): `xinput list` classification,
  exact disable-other/enable-own argv and `DISPLAY`/`XAUTHORITY` environment,
  core devices untouched, missing own device failing before any mutation,
  release re-enable/idempotence, and rollback on disable/enable failure.
- Input-targeting supervisor integration (`tests/input_targeting_supervisor.rs`):
  display → helper grab → target → node order, only-own enabled and cross-seat
  devices disabled, strict failure without spawn plus helper/display rollback,
  warn failure still spawning with a typed error and a `input_targeting_failed`
  note, shared-display no-op, stop re-enable, re-apply after helper restart and
  display restart without a stale claim, earlier-seat rollback, and a typed
  release failure.
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
- Seat persistence (`tests/persistence.rs`): atomic save/reload with no temp
  file left behind, missing-file empty load, unversioned migration, corrupt and
  newer-version typed errors, duplicate id/display/port/sink rejection, stable
  id/port/sink mapping when displays reorder, missing-display retention without
  planning, missing-hub/device diagnostics, first-unused allocation for new
  displays, and persisted per-seat profile defaults.
- Hotplug (`tests/hotplug.rs`): display add/remove start/stop, hub add/remove
  restart with missing-input diagnostics, geometry-change restart with a stable
  seat id, debounce cancellation of transient add+remove, no-op quiet polls,
  excluded display/hub/seat policy, typed monitor failure, and a sysfs fixture
  baseline/add/remove cycle.
- Per-seat audio (`tests/audio_supervisor.rs`): ensure before spawn and remove
  after stop, exact `ozma-seat-N`/`ozma-seat-N.monitor` names, per-seat sinks,
  typed non-fatal ensure failure, best-effort remove failure with seat
  retention/retry, and no audio call in dry-run.
- DRM leases (`tests/drm_lease.rs`): fake card enumeration/inspection, request
  parsing, object-id validation, create/validate/revoke/drop, already-leased,
  unknown connector/CRTC, no-DRM/no-permission/unsupported/capability-required
  typed failures, auto-virtual refusal of real cards, supervisor start/stop
  fd export and revoke, later-seat rollback, missing-backend failure, dry-run
  no-acquire, and status JSON (`drm_lease` plus `resources.drm_lease_*`).

Controller-side grouping is covered by Python tests with fake nodes:

```bash
python3 -m pytest tests/unit/controller/test_seats.py
```

They cover name/`seat_id`/`parent_node_id` grouping, multi-host separation,
online/offline state, unrelated-node exclusion, the API shape and token
redaction, optional `seat_id` registration compatibility, consuming a node's
explicit persisted seat report, typed node-error passthrough while keeping the
fallback group, the unchanged response shape when the route is absent, and
dashboard group rendering. The default fixture answers 404 for every node, so
no unit test opens a socket.

Node-side route tests use only in-memory/temp files and `tower::oneshot`; they
never spawn a seat process. They cover not-configured, a persisted plan with an
explicit `OZMA_SEAT_ID`, a retained missing display that stays `planned`, a
parse error as a typed 503, a newer-version error, and the always-external
read-only lifecycle (`start_stop:false`).

Real host checks are opt-in and `#[ignore]`d:

```bash
OZMA_MULTISEAT_REAL=1 cargo test -p ozma-multiseat --test real_backends \
  -- --ignored --nocapture
```

They enumerate the real `xrandr` output, read-only sysfs input, and DRM
connectors. They start no node and inject no input.

The live vkms lease check is explicit and `#[ignore]`d:

```bash
sudo modprobe vkms
OZMA_DRM_LIVE=1 cargo test -p ozma-multiseat --test live_drm_lease \
  -- --ignored --nocapture
```

It finds the card by the vkms sysfs/faux-bus identity, leases a connector and
CRTC, verifies `GET_LEASE`, spawns a child to prove fd inheritance, checks the
duplicate typed error, revokes, and re-acquires to prove the objects were
released. It never leases a real GPU. On hosts where systemd-logind already
holds DRM master on the vkms card, the test reports `capability_required` unless
run with master access; see `docs/phase4-drm-lease-report.md` for the dedicated
configfs run.

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

The real input-isolation smoke is separately gated and grabs one explicitly
configured device, then releases it:

```bash
OZMA_MULTISEAT_REAL_INPUT_GRAB=1 \
OZMA_MULTISEAT_INPUT_DEVICE=/dev/input/eventN \
  cargo test -p ozma-multiseat --test input_isolation \
  real_helper_grabs_and_releases_the_configured_device -- --ignored --nocapture
```

It needs permission to open/`EVIOCGRAB` the node and `/dev/uinput`. Use a
spare device or a disposable VM; the test releases it before returning. Default
tests never open `/dev/input` and never start the real helper backend.

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

The private-display smoke is separately gated and needs only `Xvfb`:

```bash
OZMA_MULTISEAT_REAL_DISPLAY=1 \
  cargo test -p ozma-multiseat --test live_display_smoke -- --ignored --nocapture
```

It starts two throwaway seats on the first free numbers at or above `:90`
(normally `:90`/`:91`), records each fake node's inherited `DISPLAY`, verifies
the X11 sockets exist, stops both through `ProcessSeatSupervisor`, and checks
the sockets and X server pids are gone. It never uses the operator's display.

Live smoke (2026-11-25): the private-display test passed with
`displays=[90, 91]`, recorded `DISPLAY` values `[":90", ":91"]`, both seats
`running` with `display_ready=true`, and both stopped cleanly; `/tmp/.X11-unix`
had no `X90`/`X91` sockets and no `Xvfb` process remained. The host had
`Xvfb`, `Xephyr` absent, `weston` absent, and `xauth`/`xdpyinfo` present.

The real `xinput` targeting path has its own throwaway-display smoke:

```bash
OZMA_MULTISEAT_REAL_TARGETING=1 \
  cargo test -p ozma-multiseat --test live_input_targeting_smoke \
  -- --ignored --nocapture
```

It starts one private `Xvfb` at `:90`, lists XInput devices with the real
`xinput`, asks `ProcessSeatInputTargeter` to target an existing device, verifies
a missing own uinput name is a typed error, and stops the server. It creates no
evdev/uinput device and never touches the operator's desktop.

Live targeting smoke (2026-11-25): `Xvfb` started at `:90`; `xinput` saw 6
devices and the targeter enabled `Virtual core pointer`, reported the typed
`uinput device(s) not visible` error for a missing `ozma-seat-...` name, released
the targeting, and stopped `:90` cleanly. No `Xvfb` process or `X90` socket
remained.

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

1. **Seat lifecycle integration.** `ozma-node` now reports the persisted plan
   read-only at `GET /api/v1/seats`, and the controller consumes it when
   present. Still missing: a manager that lets the node or controller start,
   stop, or restart seats, register supervisor runtime state, merge seats into a
   super-node, or isolate seat children (process groups/namespaces). The
   standalone CLI remains the lifecycle owner.
2. **True display ownership.** xrandr output positioning exists as a command
   model/controller, but the shared backend still has one root window and
   keyboard focus. The opt-in `--seat-display xvfb`/`xephyr` path now provides a
   real private X root and prepared `DISPLAY`/`XAUTHORITY` for each seat, but it
   is a virtual X server: physical output ownership/scanout, one compositor or
   WM per seat, DRM leasing, and Wayland compositor seats are still missing.
   Nested displays report physical-ownership and xinput mapping as typed notes,
   not fake success.
3. **Input routing execution and policy.** `xinput map-to-output` and rollback
   are invoked by the supervisor when `--apply-routing` is enabled, and
   `--input-isolation helper` now provides a real privileged `EVIOCGRAB`/uinput
   helper with rollback, cross-seat rejection, and crash re-grab. Still missing:
   udev/Xorg `InputDevice` sections, device-permission management (udev rules,
   `input`/`uinput` groups or capabilities), hotplug re-routing policy, and
   durable per-seat input state across supervisor restarts.
4. **Per-seat input consumption.** The environment names the target seat
   (`OZMA_UINPUT_SEAT`) and the helper reports the per-seat uinput device names,
   but no node/compositor integration yet consumes those virtual devices as that
   seat's input source. The helper relays physical events; injection into a
   specific X screen or compositor seat is still missing.
5. **Audio isolation.** Null-sink creation/removal and monitor targeting are
   now opt-in through `ozma-audio`/`--audio-router`; moving application streams,
   named microphone remaps, room correction/VBAN, and a Windows virtual endpoint
   are still missing.
6. **Auto-detection.** A versioned `seats.json` store, stable replan mapping, a
   polling sysfs hotplug model, debounce, and start/stop/restart reconciliation
   now exist. Still missing: a udev/netlink event source, Xrandr/DRM
   re-enumeration inside the monitor, automatic store writes outside the CLI,
   scale-up beyond discovered displays, and virtual-display-on-demand.
7. **Profiles and games.** Launcher commands/environment, Steam/Lutris/Playnite
   discovery, per-seat GPU/encoder allocation, and isolation (`user`,
   `sandboxie`, namespaces, cgroups).
8. **Per-seat streaming.** HLS/WebRTC endpoints, per-seat monitoring, and the
   controller seeing N independent registrations.
9. **Supervisor hardening.** A packaged `ozma-multiseat.service` (restart limit,
   mixed kill mode, least-privilege sandbox) now exists, but process-group/
   subreaper teardown, log capture/rotation, restart-count limits beyond
   systemd's, readiness for non-HTTP nodes, and child resource limits are still
   missing.
10. **Windows.** DXGI/SetupAPI display enumeration, IddCx virtual displays, DXGI
    Desktop Duplication per output, Raw Input routing, WASAPI endpoints, and
    XInput gamepad mapping are **not implemented or claimed**.
11. **macOS.** Not claimed.

The Python `agent/multiseat` package remains the reference for behavior and is
not being deleted by this crate.
