# Routing Protocol Specification

**Version**: 0.1 (unstable)
**Status**: Draft — defines the model from which all other protocol work derives

---

## 1. Overview

Ozma is a software-defined KVMA router. The routing protocol defines how video,
audio, and peripheral signals are modelled, discovered, negotiated, and delivered
across an arbitrarily complex graph of devices, transports, and conversions.

The core premise: every signal path — from a keyboard through a USB cable, across
a network, into a USB gadget on a target machine — is a graph of typed ports
connected by links. Each link has measurable properties (bandwidth, latency,
jitter, loss). Each port has discoverable capabilities (supported formats,
maximum throughput). The router's job is to assemble the best pipeline through
this graph to satisfy a declared **intent**.

**The graph model is valuable independent of KVM switching.** A single PC
running only the desktop agent — no nodes, no target machines, no switching —
still benefits from the routing graph. It provides: USB topology with
controller mapping and bandwidth sharing detection, physical port
recommendations, PipeWire audio routing with mix bus and per-app separation,
Bluetooth codec negotiation awareness, WiFi quality monitoring, power
tracking, device database matching (dock internal topology, GPU codec
capabilities), storage health trending, thermal monitoring, and a unified
dashboard with historical journal and trend alerts. The KVMA router is the
headline feature; the graph model is the platform everything else builds on.

This specification defines:

1. **Graph primitives** — the vocabulary for describing the signal fabric
2. **Intents** — what the user wants to achieve, and the constraints that implies
3. **Format system** — how media capabilities are described and negotiated
4. **Information quality** — how to trust (or not) reported properties
5. **Route calculation** — how pipelines are assembled and re-evaluated
6. **Plugin contracts** — registration, lifecycle, interfaces for transports, devices, codecs, converters
7. **Clock model** — distributed timing across independent devices
8. **Topology discovery** — what can be known, from where, and at what confidence
9. **Device versioning and mesh updates** — version tracking, OTA delivery, fleet orchestration
10. **Power model** — voltage rails, current budgets, measurement, USB PD, PoE, RGB power limiting
11. **Furniture and physical environment** — desks, racks, rooms, sites, relative positioning, zone types
12. **Control path** — how commands reach devices, dependency chains, reachability, fallback paths
13. **Audio routing model** — mix buses, monitor controller, insert chains, spatial audio, metering, gain staging
14. **Physical device database** — universal open catalog with motherboard/CPU/chipset topology, hosted on Connect
15. **Node definition** — complete specification of what a node is: hardware, USB gadget, physical I/O, services, lifecycle

This document does not define wire formats or byte layouts — those live in
individual protocol specs (`protocol/specs/`). This document defines the
**model** that those specs implement.

---

## 2. Graph Primitives

The routing graph is composed of four primitives: **Device**, **Port**, **Link**,
and **Pipeline**. Together they describe the complete signal fabric from any
source to any destination.

### 2.1 Device

A device is any physical or virtual thing that has ports. Devices form the nodes
of the routing graph.

```yaml
Device:
  id: string                    # globally unique (derived from hardware identity or assigned)
  name: string                  # human-readable label
  type: enum                    # see Device Types below
  location: Location            # where this device is in the topology
  ports: Port[]                 # all ports on this device
  internal_links: Link[]        # connections between ports within the device
  properties: PropertyBag       # device-specific metadata
  topology: DeviceTopology?     # internal structure (for compound devices like docks)
  controllability: Controllability? # how (if at all) this device can be commanded (see §2.5)
  control_path: ControlPath?    # how commands reach this device (see §2.12)
  capacity: DeviceCapacity?     # resource limits and current usage (see §2.7)
  resource_budget: ResourceBudget? # maximum resources Ozma may consume on this device (see §2.7)
  power_profile: DevicePowerProfile? # power consumption, delivery, and rail state (see §2.10)
  version: DeviceVersion?       # software/firmware version and update state (see §14)
```

**Device types**:

| Type | Examples |
|------|----------|
| `controller` | Ozma controller process |
| `node` | Hardware node (SBC), soft node, desktop agent |
| `target` | The machine a node is connected to (see §2.9 for VM targets) |
| `capture_card` | V4L2 capture device, USB or PCIe |
| `display` | Monitor, projector (full-size displays, DDC/CI controllable) |
| `screen` | Stream Deck LCD, Corsair iCUE screen, OLED/e-ink module, LED matrix panel (see §2.8) |
| `audio_interface` | USB audio device, PipeWire node, ALSA device |
| `audio_processor` | Room correction filter, EQ, ducking engine, voice detector (see §2.9) |
| `usb_hub` | USB hub (including those embedded in docks) |
| `usb_controller` | Host controller (xHCI, EHCI) |
| `network_interface` | Ethernet, WiFi, WireGuard tunnel, WiFi AP |
| `dock` | Thunderbolt/USB-C dock (compound device with internal topology) |
| `codec` | Hardware encoder/decoder (Quick Sync, VCN, NVENC) |
| `software_codec` | Software encoder/decoder (ffmpeg, GStreamer element) |
| `peripheral` | Keyboard, mouse (input-only devices) |
| `control_surface` | MIDI controller, gamepad, Stream Deck buttons, ShuttlePRO, foot pedal, OSC device (see §2.8) |
| `rgb` | WLED strip, addressable LED strip, per-key keyboard RGB, case lighting, Art-Net fixture (see §2.8) |
| `camera` | Webcam, IP camera (RTSP/ONVIF/NDI), virtual camera, doorbell camera (see §2.9) |
| `speaker` | Audio output device |
| `microphone` | Audio input device, room mic |
| `phone` | Mobile phone connected via USB (UAC2, ADB, tethering, screen mirror) or KDE Connect (see §2.9) |
| `actuator` | Monitor stand, sit/stand desk, crane, linear actuator (see §2.9) |
| `sensor` | Temperature, current, humidity, motion, door contact (see §2.9) |
| `ups` | UPS via NUT — battery, load, runtime, power state |
| `relay` | Connect relay endpoint |
| `switch` | KVM switch, HDMI matrix, audio matrix, crosspoint switch |
| `vm_host` | QEMU/KVM/Proxmox hypervisor (manages VM targets) (see §2.9) |
| `service` | Managed service (Frigate, Jellyfin, Immich, Vaultwarden, HA) (see §2.9) |
| `media_receiver` | Spotify Connect (librespot), AirPlay (shairport-sync), Chromecast, DLNA renderer |
| `media_source` | Jellyfin, Plex, Tidal Connect, local media library — controllable content sources |
| `notification_sink` | Webhook, Slack, Discord, email, Pushover, ntfy |
| `metrics_sink` | Prometheus, Datadog, InfluxDB, syslog exporter |
| `furniture` | Motorised desk, monitor arm — furniture that is also an actuator (see §2.11) |
| `network_switch` | Managed/unmanaged Ethernet switch, PoE switch (see §2.9) |
| `router` | Network router, gateway, firewall (see §2.9) |
| `access_point` | WiFi access point (standalone or integrated in router) |
| `virtual` | Any software-defined device (loopback, pipe, virtual display, macro source) |

A device may be **compound** — a Thunderbolt dock contains a USB hub, an
Ethernet adapter, a display output, and an audio codec, each modelled as a
sub-device with their own ports, connected by internal links. The `topology`
field expresses this internal structure.

A device may be **switchable** — an external KVM switch or HDMI matrix has
multiple input ports and one or more output ports, with a configurable internal
routing matrix. See §2.5 for the switch model.

**Location** — every entity in the graph has a location, which has both a
logical component (bus topology, network address) and a physical component
(where it is in the real world). Physical location is optional but enables
spatial routing (§8.1), zone inference, and the 3D scene.

```yaml
Location:
  # --- Logical (bus/network topology) ---
  machine_id: string?           # which physical/virtual machine this device is on
  bus: string?                  # "usb", "pcie", "network", "internal"
  bus_path: string?             # OS-reported path (e.g., "1-2.3" for USB)
  overlay_ip: string?           # WireGuard overlay address if applicable

  # --- Physical (real-world position) ---
  physical: PhysicalLocation?   # where this entity is in the physical world
```

**PhysicalLocation** — where something is in the real world:

```yaml
PhysicalLocation:
  # Absolute position (if known)
  site: string?                 # site/building ("home", "office-hq", "datacentre-east")
  space: string?                # room/area ("study", "living_room", "server_closet", "desk_area")
  zone: string?                 # spatial zone reference (§8.1 SpatialZone.id)

  # Position (in world-space coordinates, mm — see device-db.md for coordinate system)
  pos: Position3d?              # { x, y, z } in world space
  rot: Rotation3d?              # { yaw, pitch, roll } in degrees

  # Relative position (defined in relation to another entity)
  relative_to: RelativeLocation?

  # Metadata
  placement: string?            # "on_desk", "under_desk", "wall_mounted", "rack_mounted",
                                # "floor_standing", "ceiling_mounted", "carried", "in_pocket"
  mobile: bool?                 # does this entity move? (phone, laptop, wireless peripheral)
  quality: InfoQuality          # how we know the position ("user" = placed in layout editor,
                                # "measured" = BLE/UWB, "assumed" = default)
```

**RelativeLocation** — position defined relative to another entity. This is
how most physical locations are actually specified. A keyboard isn't at
coordinates (450, 300, 720) — it's "on the desk, centered":

```yaml
RelativeLocation:
  parent_id: string             # entity this is positioned relative to
  parent_type: string           # "furniture", "device", "zone", "space"
  relationship: string          # how it relates to the parent
  offset: Position3d?           # offset from parent's origin (mm)
  slot: string?                 # named slot on the parent (e.g., "surface", "left_side",
                                # "monitor_arm_1", "shelf_2", "rack_unit_12")
```

**Relationship types**:

| Relationship | Meaning | Example |
|-------------|---------|---------|
| `on` | Sitting on the parent's surface | Keyboard on desk |
| `under` | Below the parent | PC tower under desk |
| `mounted_on` | Attached to the parent | Monitor on arm, strip on wall |
| `inside` | Contained within the parent | GPU inside PC case, drive in NAS |
| `beside` | Adjacent to the parent | Speaker beside monitor |
| `above` | Over the parent | Shelf above desk, overhead light |
| `behind` | Behind the parent | Cables behind desk, bias lighting behind monitor |
| `in_front_of` | In front of the parent | Keyboard in front of monitor |
| `attached` | Physically connected (not just sitting on) | Webcam on monitor top |

When a parent moves (desk height changes, monitor arm repositions), all
entities with `relative_to` pointing at that parent move with it. This is
the same grouping model as case groups in device-db.md, but generalised to
any entity.

**Everything has a location**: The `location` field exists on Device (§2.1)
and is inherited by all ports and links. But physical location also applies
to entities that aren't devices in the routing sense:

- Furniture (desks, chairs, shelves, racks) — see §2.11
- Rooms and spaces
- Cable runs
- Wall-mounted items that aren't electronic (whiteboards, acoustic panels)
- People (inferred zone, not tracked position)

These non-device entities exist in the physical model but not in the routing
graph. They participate in the 3D scene and in zone/location inference, but
the router doesn't build pipelines through them.

### 2.2 Port

A port is a typed endpoint on a device. Ports are directional — they are either
sources (produce data) or sinks (consume data). A port describes what it can
accept or produce.

```yaml
Port:
  id: string                    # unique within the device
  device_id: string             # parent device
  direction: source | sink
  media_type: enum              # video, audio, hid, data, power, mixed
  capabilities: FormatSet       # all formats this port can handle (see §4)
  current_state: PortState      # what the port is currently doing
  properties: PropertyBag       # port-specific metadata
  latency: LatencySpec          # see §7
  physical: PhysicalPortInfo?   # where this port is on the device's body (see §15)
```

**PortState**:

```yaml
PortState:
  active: bool                  # is this port currently in use
  current_format: Format?       # the format currently flowing (null if inactive)
  current_bandwidth: Measurement? # measured throughput
  connected_to: PortRef[]       # which ports this is linked to
```

Ports exist at every boundary where a signal changes form or transport:

- An HDMI output on a GPU is a video source port
- An HDMI input on a capture card is a video sink port
- A USB bulk endpoint on a capture card is a video source port (different format)
- A V4L2 device node is a video source port (same device, OS-level interface)
- A UDP socket is a source or sink port for network transport
- A PipeWire node pad is an audio source or sink port
- A `/dev/hidg0` device is an HID sink port (writes to USB gadget)

The same physical device typically has multiple ports at different abstraction
levels. A capture card has an HDMI sink port (physical input), an internal link
to a USB source port (hardware output), and a V4L2 source port (OS interface).
The routing graph includes all of them — the router chooses which level to
connect at based on what's available and what the pipeline requires.

### 2.3 Link

A link is a connection between a source port and a sink port. Links carry data
and have measurable properties.

```yaml
Link:
  id: string                    # unique
  source: PortRef               # device_id + port_id
  sink: PortRef                 # device_id + port_id
  transport: string             # transport plugin id (see §6.1)
  direction: unidirectional | bidirectional
  state: LinkState
  properties: PropertyBag
```

**LinkState**:

```yaml
LinkState:
  status: active | warm | standby | failed | unknown
  format: Format?               # negotiated format on this link (null if standby)
  bandwidth: BandwidthSpec      # capacity and current usage
  latency: LatencySpec          # measured or estimated — steady-state per-packet/frame delay
  jitter: JitterSpec            # measured
  loss: LossSpec                # measured packet/frame loss
  activation_time: ActivationTimeSpec  # time to transition this link between states (see §2.6)
  last_measured: timestamp      # when properties were last updated
```

Link status values:
- `active` — data is flowing through this link right now
- `warm` — link is initialised and ready (process running, connection established,
  negotiation complete) but no data is flowing. Activation from warm is near-zero.
- `standby` — link exists in the graph but is not initialised. Activation requires
  startup (process launch, negotiation, handshake).
- `failed` — link was active or warm but has broken
- `unknown` — state cannot be determined

**BandwidthSpec**:

```yaml
BandwidthSpec:
  capacity_bps: uint64          # maximum theoretical bandwidth
  available_bps: uint64         # currently available (capacity minus other users)
  used_bps: uint64              # currently consumed by this link's data
  quality: InfoQuality          # see §5
```

**LatencySpec**:

```yaml
LatencySpec:
  min_ms: float                 # best case
  typical_ms: float             # median / p50
  max_ms: float                 # worst case / p99
  quality: InfoQuality
```

**JitterSpec**:

```yaml
JitterSpec:
  mean_ms: float
  p95_ms: float
  p99_ms: float
  quality: InfoQuality
```

**LossSpec**:

```yaml
LossSpec:
  rate: float                   # 0.0–1.0, fraction of packets/frames lost
  window_seconds: uint          # measurement window
  quality: InfoQuality
```

**ActivationTimeSpec** (see §2.6 for full discussion):

```yaml
ActivationTimeSpec:
  cold_to_warm_ms: float        # time to initialise (process start, negotiation, HDCP, etc.)
  warm_to_active_ms: float      # time to start data flow once initialised
  active_to_warm_ms: float      # time to stop data but keep ready
  warm_to_standby_ms: float     # time to tear down
  quality: InfoQuality
```

Links can be:

- **Physical**: HDMI cable, USB cable, audio cable
- **Logical**: Network path (UDP between two hosts), WireGuard tunnel, Connect relay
- **Virtual**: Software pipe, PipeWire link, loopback
- **Exotic**: HDMI output → HDMI capture (loopback transport), USB gadget composite

A link between two ports on the **same device** is an internal link. Internal
links model the signal path through compound devices (e.g., HDMI input → USB
output inside a capture card). Their properties are typically `spec` or
`reported` quality.

### 2.4 Pipeline

A pipeline is an ordered chain of links assembled by the router to carry a
signal from a source to a destination, satisfying a declared intent.

```yaml
Pipeline:
  id: string
  intent: Intent                # what this pipeline satisfies (see §3)
  source: PortRef               # origin port
  destination: PortRef          # terminal port
  hops: PipelineHop[]           # ordered list of links and conversions
  aggregate: PipelineMetrics    # computed from hops
  state: active | warm | standby | failed
  warmth_policy: WarmthPolicy   # should this pipeline be kept warm? (see §2.6)
```

**PipelineHop**:

```yaml
PipelineHop:
  link: LinkRef                 # the link traversed
  input_format: Format          # format entering this hop
  output_format: Format         # format leaving this hop
  conversion: ConversionRef?    # if formats differ, the converter used
  latency_contribution_ms: float # this hop's steady-state latency
  activation_time: ActivationTimeSpec  # this hop's state transition times
  current_state: active | warm | standby | failed
```

**PipelineMetrics** (computed, not stored — derived from hop properties):

```yaml
PipelineMetrics:
  total_latency_ms: float       # sum of all hop latencies (steady-state)
  bottleneck_bandwidth_bps: uint64  # minimum available bandwidth across all hops
  total_conversions: uint       # number of format changes (each adds latency + potential quality loss)
  total_hops: uint              # number of links traversed
  end_to_end_jitter_ms: float   # aggregated jitter
  end_to_end_loss: float        # compound loss probability
  weakest_quality: InfoQuality  # lowest confidence level across all hops
  activation_time_ms: float     # time to go from current state to active (see §2.6)
  warm_activation_time_ms: float # time to activate if pipeline is warm
  cold_activation_time_ms: float # time to activate from fully cold/standby
```

A pipeline is a **computed artefact**, not a stored configuration. The router
computes pipelines from the graph, the available links, and the declared intent.
Pipelines are re-evaluated when the graph changes (device added/removed, link
metrics change, intent changes).

**Multi-stream**: A single user action (e.g., switching to a scenario) may
require multiple pipelines — one for video, one for audio, one for HID. These
are independent pipelines with independent intents. They may share some links
(same network path) but are negotiated separately.

**Fan-out**: A single source port can feed multiple pipelines simultaneously.
Each pipeline has its own intent and may negotiate a different format. The source
produces once; conversion happens per-pipeline where needed. Fan-out is how
preview thumbnails, recording, and broadcast coexist with the primary user
session.

### 2.5 External Switches

External switches — KVM switches, HDMI matrix switches, audio matrix switches,
crosspoint switches — are devices with multiple input ports and one or more
output ports, where the internal routing between them is configurable. They are
first-class members of the routing graph.

**Controllability**:

Not all switches are equal. Some can be read and written, some can only be
commanded with no feedback, and some are entirely manual (the user presses a
physical button). The `controllability` field on the Device captures this:

```yaml
Controllability:
  state_readable: bool          # can we query current routing state?
  state_writable: bool          # can we command routing changes?
  feedback: FeedbackModel       # what kind of confirmation do we get?
  control_interface: string?    # how we talk to it ("serial", "ir", "ip", "usb", "cec", "manual")
  control_plugin: string?       # plugin id that handles this device

FeedbackModel: enum
  confirmed       # device reports current state (readable + writable)
  write_only      # we can command it but never confirm (IR blaster, many serial devices)
  manual          # no electronic control — user operates it physically
  event_only      # device emits state changes but cannot be commanded
```

**Switch routing matrix**:

A switch has a configurable internal routing matrix — which input ports are
connected to which output ports. This is modelled as **switchable internal
links**:

```yaml
SwitchMatrix:
  device_id: string
  routes: SwitchRoute[]
  matrix_type: one_to_one | many_to_one | many_to_many

SwitchRoute:
  input_port: PortRef           # input port on the switch
  output_port: PortRef          # output port on the switch
  active: bool                  # is this route currently active?
  state_quality: InfoQuality    # how confident are we in this state?
```

For a `write_only` device, `state_quality` is `commanded` — we sent the switch
command but have no confirmation it was applied. The router must account for
this uncertainty.

For a `confirmed` device, `state_quality` is `reported` — the device told us
its current state.

For a `manual` device, `state_quality` is `assumed` after the user tells us
what they set, or `user` if they explicitly confirmed it.

**Router behaviour with switches**:

1. **Confirmed switches**: The router treats switchable internal links like any
   other link. It can activate routes, read state, and trust the response.

2. **Write-only switches**: The router sends the switch command and marks the
   internal link as active with `commanded` quality. If the pipeline then fails
   (e.g., no video arriving when expected), the router knows the switch state
   is uncertain and can retry or alert the user.

3. **Manual switches**: The router cannot activate routes — it can only
   recommend. The pipeline is marked as requiring user action. Once the user
   confirms the switch position, the route is marked `user` quality.

4. **Event-only switches**: The router observes state changes (e.g., a KVM
   switch with hotkey detection that reports which input is active) but cannot
   command changes. Useful for integrating existing infrastructure the user
   controls manually but Ozma should be aware of.

**Examples**:

| Device | Controllability | Typical interface |
|--------|----------------|-------------------|
| TESmart HDMI matrix | confirmed | Serial (RS-232) or IP |
| Cheap HDMI switch with IR | write_only | IR blaster |
| HDMI CEC-capable TV | confirmed | CEC over HDMI |
| Manual desktop KVM | manual | Physical button |
| Enterprise Extron matrix | confirmed | Serial or IP, full status feedback |
| AV receiver (HDMI inputs) | confirmed or write_only | IP, serial, or CEC |

**Switch as bridge**: A switch in the graph acts as a bridge between otherwise
disconnected segments. If Machine A's HDMI output goes through an HDMI matrix
to a capture card on Machine B, the routing graph shows:

```
Machine A: GPU:hdmi-out → hdmi-cable → Matrix:input-3
Matrix: input-3 → [switchable internal link] → output-1
Matrix: output-1 → hdmi-cable → Capture Card:hdmi-in
Capture Card: hdmi-in → internal → Capture Card:usb-out → ...
```

The router can activate `input-3 → output-1` on the matrix (if writable) as
part of assembling the pipeline. The switch command is part of pipeline
activation, not a separate operation.

### 2.6 Activation Time and Pipeline Warmth

Steady-state latency (how long each packet/frame takes to traverse a link) is
fundamentally different from activation time (how long it takes to bring a link
from cold to flowing). The routing protocol tracks both separately because they
affect different decisions:

- **Latency** determines whether a pipeline can satisfy an intent's real-time
  requirements once running.
- **Activation time** determines how quickly the user experiences a response
  when switching scenarios, and whether pipelines should be kept warm.

#### Activation time

Every link and every device has activation time properties describing how long
state transitions take:

```yaml
ActivationTimeSpec:
  cold_to_warm_ms: float        # initialise: start process, negotiate, handshake
  warm_to_active_ms: float      # begin data flow from ready state
  active_to_warm_ms: float      # stop data but keep initialised
  warm_to_standby_ms: float     # tear down, release resources
  quality: InfoQuality          # provenance of these estimates
```

**Examples of activation time**:

| Component | cold→warm | warm→active | Notes |
|-----------|-----------|-------------|-------|
| UDP socket | ~0ms | ~0ms | Already bound, just start sending |
| ffmpeg process | 1000–3000ms | ~0ms | Process start + codec init + first keyframe |
| ffmpeg (already running) | 0ms | ~0ms | Warm — just start reading frames |
| HDMI capture card | 100–500ms | ~0ms | Signal lock + EDID negotiation |
| KVM switch (HDMI) | 1000–3000ms | ~0ms | Input switch + HDCP re-auth + EDID |
| HDMI matrix (fast) | 200–500ms | ~0ms | Input switch, minimal re-negotiation |
| WireGuard tunnel | 50–200ms | ~0ms | Handshake (already have keys) |
| Connect relay | 200–1000ms | ~0ms | Relay allocation + WireGuard through relay |
| PipeWire link | ~0ms | ~0ms | Kernel-level, near-instant |
| Sunshine session | 2000–5000ms | ~0ms | RTSP + HDCP + codec negotiation |
| USB gadget (configfs) | 500–2000ms | ~0ms | Gadget creation + host enumeration |
| Manual KVM switch | ∞ (user action) | ~0ms | Unpredictable — user must act |

Activation time is additive across a pipeline, but only for hops that aren't
already warm. The pipeline's activation time is determined by its **slowest
cold hop**, not the sum — hops can initialise in parallel where there are no
dependencies:

```
pipeline_activation_ms = max(cold_hop_activation_times)  # parallel init
                       + sum(serial_dependency_times)     # where hop N needs hop N-1 first
```

For example: switching to a scenario that goes through an HDMI matrix (2s) and
needs ffmpeg (3s) — if they can init in parallel, activation is ~3s, not 5s.
But if ffmpeg can't start until the matrix has switched (because it needs the
video signal), activation is 2s + 3s = 5s.

#### Pipeline warmth

To avoid activation time on scenario switches, pipelines can be kept **warm** —
initialised and ready but not actively flowing data. This trades resources
(running processes, open connections, memory) for faster switching.

```yaml
WarmthPolicy:
  keep_warm: bool               # should this pipeline be kept warm when not active?
  warm_priority: uint           # if resources are limited, which warm pipelines to keep
  max_warm_duration_s: uint?    # auto-cool after this long (null = indefinite)
  warm_cost: WarmCost           # resource cost of keeping this pipeline warm
```

**WarmCost** (informational — helps the router decide what to keep warm):

```yaml
WarmCost:
  cpu_percent: float?           # estimated CPU usage while warm (idle process)
  memory_mb: float?             # estimated memory usage while warm
  bandwidth_bps: uint64?        # any keepalive traffic
  gpu_slots: uint?              # hardware codec sessions held open
  description: string?          # human-readable cost summary
```

**Who decides what stays warm?**

1. **Scenario-driven**: The router keeps pipelines warm for scenarios the user
   is likely to switch to. If the user has two scenarios (Desktop and Gaming),
   the router keeps the inactive one's video pipeline warm (ffmpeg running,
   reading frames, discarding them — ready to stream instantly on switch).

2. **Intent-driven**: Intents can declare activation time constraints:

```yaml
Constraints:
  max_activation_time_ms: float?  # pipeline must activate within this time
```

If an intent says `max_activation_time_ms: 100`, the router will only select
pipelines that are already warm (or have cold activation under 100ms). This
forces pre-warming.

3. **Explicit**: The user or automation can explicitly mark pipelines to keep
   warm via the API:

```
POST /api/v1/routing/pipelines/{id}/warm    # warm this pipeline
POST /api/v1/routing/pipelines/{id}/cool    # release warm resources
GET  /api/v1/routing/pipelines/warm         # list all warm pipelines with costs
```

4. **Automatic**: The router can auto-warm pipelines based on usage patterns.
   If the user switches between two scenarios frequently, the router learns to
   keep both warm. If a pipeline hasn't been used in hours, it cools
   automatically (governed by `max_warm_duration_s`).

**Warming strategies by component**:

| Component | How to keep warm | Cost |
|-----------|-----------------|------|
| ffmpeg | Start process, decode to /dev/null | CPU + memory (~50–200 MB) |
| Capture card | Keep signal locked, discard frames | Minimal (hardware idle) |
| HDMI matrix | No warmth concept — switching is the cost | N/A |
| KVM switch | Pre-switch during warm phase (if writable) | Disrupts current user briefly |
| WireGuard | Keep tunnel established, send keepalives | ~1 KB/s keepalive |
| Connect relay | Keep relay session allocated | ~1 KB/s keepalive |
| Sunshine | Keep RTSP session, pause stream | Memory + GPU session slot |
| UDP transport | Keep socket bound | Negligible |

**KVM switch warming**: External switches present a special case. You can't
"warm" a switch without actually switching it. For KVM switches with HDCP
negotiation delay, the activation time is unavoidable. The router accounts for
this honestly — if the pipeline goes through a slow switch, the activation time
is reported as-is. The user can mitigate this by using a faster switch, removing
the switch from the path, or accepting the delay.

For HDMI matrices with multiple outputs, the router can potentially pre-route an
unused output to the target input — warming the HDCP handshake on a different
output port — but this depends on the matrix having spare outputs.

### 2.7 Device Capacity and Resource Pressure

Every device in the graph has finite resources. A Raspberry Pi has different
capacity from an Intel N100, which has different capacity from a workstation
with an RTX 4090. The routing protocol must track these limits so the router
doesn't overcommit a device — and so software agents don't burden the machines
they run on.

#### Capacity model

```yaml
DeviceCapacity:
  resources: ResourcePool[]     # all resource types available on this device
  current_load: ResourceUsage   # what's being consumed right now
  reserved: ResourceUsage       # what's reserved by Ozma (active + warm pipelines)
  available: ResourceUsage      # capacity - current_load (what's actually free)
```

**ResourcePool** — a single resource dimension with capacity and current state:

```yaml
ResourcePool:
  type: ResourceType            # what kind of resource
  capacity: float               # total available (units depend on type)
  current_usage: float          # currently in use (all consumers, not just Ozma)
  ozma_usage: float             # usage attributable to Ozma pipelines
  other_usage: float            # usage by non-Ozma processes
  quality: InfoQuality          # how we know these numbers
```

**ResourceType**:

```yaml
ResourceType: enum
  cpu_percent                   # CPU utilisation (0–100 per core, or total)
  cpu_cores                     # number of cores available
  memory_mb                     # RAM in megabytes
  gpu_percent                   # GPU compute utilisation
  gpu_memory_mb                 # GPU VRAM
  gpu_encode_slots              # hardware encoder sessions (NVENC, QSV, VCN)
  gpu_decode_slots              # hardware decoder sessions
  disk_iops                     # storage I/O operations per second
  disk_bandwidth_mbps           # storage throughput
  usb_bandwidth_bps             # aggregate USB bus bandwidth (per controller)
  network_bandwidth_bps         # network interface throughput
  thermal_headroom_c            # degrees below thermal throttle threshold
  power_rail_capacity_ma        # per-rail current capacity (see §2.10)
  power_rail_usage_ma           # per-rail current draw (measured or inferred)
```

Not every device reports every resource type. An SBC node might report CPU,
memory, and thermal headroom. A capture card reports nothing (it's a fixed-
function device — its limits are expressed as port capabilities). A GPU codec
reports encode/decode slots. The router works with whatever is available and
applies `assumed` quality for missing data where it matters.

#### Resource cost of pipeline operations

Every pipeline operation consumes resources on the device it runs on. The
router tracks this:

```yaml
ResourceCost:
  device_id: string             # which device is loaded
  costs: ResourceDemand[]       # per-resource-type demand

ResourceDemand:
  type: ResourceType
  active_cost: float            # resource consumed while pipeline is active
  warm_cost: float              # resource consumed while pipeline is warm
  peak_cost: float?             # transient peak during activation (e.g., codec init)
  quality: InfoQuality          # measured, estimated, or assumed
```

**Example resource costs**:

| Operation | Device | Resource | Active | Warm | Peak |
|-----------|--------|----------|--------|------|------|
| ffmpeg H.264 encode 1080p30 | Node (N100) | cpu_percent | 35% | 2% | 60% |
| ffmpeg H.264 encode 1080p30 | Node (N100) | memory_mb | 180 | 180 | 250 |
| NVENC H.264 encode 1080p60 | Workstation GPU | gpu_encode_slots | 1 | 1 | 1 |
| NVENC H.264 encode 1080p60 | Workstation GPU | gpu_percent | 5% | 0% | 10% |
| V4L2 capture 1080p60 | Node (Pi 5) | cpu_percent | 8% | 1% | 15% |
| V4L2 capture 1080p60 | Node (Pi 5) | usb_bandwidth_bps | 200M | 0 | 200M |
| VBAN audio stream | Node | cpu_percent | 1% | 0% | 2% |
| VBAN audio stream | Node | network_bandwidth_bps | 1.5M | 0 | 1.5M |
| Ozma desktop agent (idle) | Target PC | cpu_percent | 0.5% | — | 2% |
| Ozma desktop agent (capture) | Target PC | cpu_percent | 8% | 1% | 15% |
| Ozma desktop agent (capture) | Target PC | memory_mb | 120 | 50 | 200 |
| PipeWire audio routing | Controller | cpu_percent | 1% | 0.5% | 3% |
| 3 warm preview pipelines | Node (Pi 5) | memory_mb | 540 | 540 | 540 |

Resource costs can be `assumed` (from a lookup table based on hardware class),
`spec` (from codec documentation), or `measured` (from actual observation). The
router prefers `measured` and refines estimates over time.

#### Resource pressure and routing decisions

The router uses device capacity in three ways:

**1. Constraint checking**: A pipeline is rejected if activating it would exceed
any device's capacity. This is checked during path computation (§8.3):

```
For each device touched by a candidate pipeline:
  For each resource type:
    if (device.current_usage + pipeline.resource_cost) > device.capacity:
      reject this pipeline
```

**2. Cost weighting**: Even when a device has headroom, high utilisation is
penalised. A node at 80% CPU is a worse candidate for an additional encode
job than one at 20%:

```
cost(hop) += w_pressure × (device.current_usage / device.capacity)  # per resource type
```

This makes the router naturally load-balance across devices when multiple paths
exist.

**3. Warm pipeline eviction**: When device resources are contended, warm
pipelines are evicted in priority order (lowest `warm_priority` first). The
router cools pipelines until the device has headroom for the active workload.

#### Resource budgets and agent courtesy

Software agents running on target machines (desktop agent, soft node agent)
must not place an undue burden on the machine's primary workload. The
`resource_budget` field on a device defines the maximum resources Ozma is
allowed to consume:

```yaml
ResourceBudget:
  limits: ResourceLimit[]       # per-resource-type caps
  mode: budget_mode             # how strictly to enforce
  source: user | auto | default # who set this budget

ResourceLimit:
  type: ResourceType
  max_value: float              # absolute maximum Ozma may consume
  max_percent: float?           # alternative: percentage of total capacity
  backoff_threshold: float?     # start reducing at this level (soft limit)
  hard_limit: float             # never exceed this (hard limit)

BudgetMode: enum
  strict                        # hard enforcement — reject pipelines that exceed budget
  adaptive                      # reduce quality/framerate to stay within budget
  advisory                      # report pressure but don't restrict (user monitors)
```

**Default budgets by device type**:

| Device type | Default mode | CPU | Memory | GPU encode | Rationale |
|-------------|-------------|-----|--------|------------|-----------|
| `controller` | strict | 80% | 80% | all slots | Dedicated to Ozma — use most resources |
| `node` (SBC) | strict | 90% | 90% | all slots | Dedicated hardware — use almost everything |
| `target` (workstation) | adaptive | 10% | 200 MB | 1 slot | User's machine — be invisible |
| `target` (server) | adaptive | 15% | 500 MB | 2 slots | More headroom, but still secondary |
| `target` (kiosk) | strict | 30% | 500 MB | 2 slots | Dedicated purpose, but not Ozma's |

Users can override these defaults per device. A gaming PC's budget might be set
to 5% CPU (Ozma should be invisible during gaming) while a media server might
allow 25% (encoding is expected).

**Adaptive budget enforcement**: When the budget mode is `adaptive`, the agent
and router collaborate to stay within limits:

1. Agent monitors its own resource consumption against its budget
2. If approaching `backoff_threshold`, agent signals the controller
3. Controller degrades the pipeline: lower resolution, lower framerate, disable
   warm pipelines on this device
4. If the device's primary workload drops (user stops gaming), the agent
   signals headroom available, controller restores quality

This is continuous, not one-shot. The agent reports resource pressure
periodically (via the existing device metrics system), and the router adjusts
pipeline parameters in response.

**Peak load protection**: During activation (process startup, codec init),
resource usage can spike transiently above the steady-state cost. The
`peak_cost` field captures this. The router checks that the device can
absorb the peak before activating a pipeline — even if steady-state usage
is within budget, a startup spike that starves the user's workload is
unacceptable.

**Multi-pipeline resource accounting**: When multiple pipelines traverse the
same device, their resource costs are summed. Three warm preview pipelines on a
Pi 5 might each use 180 MB of memory — the router knows the device has 4 GB and
accounts for all three, not just one. If a fourth pipeline would push memory
over capacity, it's rejected or an existing warm pipeline is evicted.

#### Resource discovery

Resource capacity is discovered from the OS and hardware:

| Resource | Linux | Windows | macOS |
|----------|-------|---------|-------|
| CPU cores/speed | `/proc/cpuinfo`, `lscpu` | WMI | `sysctl` |
| Memory | `/proc/meminfo` | WMI | `sysctl` |
| GPU info | `nvidia-smi`, `vainfo`, `intel_gpu_top` | DXGI, NVAPI | `system_profiler` |
| GPU encode slots | Driver API (NVENC: `NvEncGetEncodeCaps`) | Same | VideoToolbox |
| USB bandwidth | `lsusb -t` (speed class) | WMI | `system_profiler` |
| Thermal | `sensors`, sysfs thermal zones | WMI | `powermetrics` |
| Current usage | `/proc/stat`, `nvidia-smi` | PDH counters | `host_processor_info` |

Current usage is sampled periodically (default: every 5 seconds for idle
devices, every 1 second for devices with active pipelines). The reporting
interval is adaptive — more frequent when the device is under pressure,
less frequent when idle.

### 2.8 Endpoint Devices: Screens, RGB, and Control Surfaces

Screens, RGB lighting, and control surfaces are endpoint devices in the routing
graph. They have ports, capabilities, resource requirements, and connection
constraints — the same as any other device. The router must account for them
when assembling pipelines and managing device pressure.

#### Screens

A screen is any visual output device that is not a full-size monitor or
projector. Screens are data sinks — they consume rendered frames or widget
definitions.

```yaml
ScreenEndpoint:
  panel_type: string            # "lcd", "oled", "eink", "led_matrix", "vfd"
  resolution: Resolution        # native panel resolution { w, h }
  color_depth: uint             # bits per pixel
  max_framerate: float          # maximum refresh rate
  response_time_ms: float?      # panel response time (e-ink: ~500ms, OLED: <1ms)
  brightness_nits: float?       # if known
  viewing_angle: float?         # degrees
  touch: bool                   # does the panel accept touch input?
  rendering_tier: uint          # 0, 1, or 2 (see below)
  connection: ConnectionInfo    # how this screen connects
```

**Rendering tiers** determine where rendering happens and what format the
screen's sink port accepts:

| Tier | Rendering location | Sink format | Latency | Resource cost |
|------|-------------------|-------------|---------|---------------|
| 0 — Push frame | Controller renders, pushes raw/JPEG frames | `screen` (raw_rgb, jpeg) | Depends on controller | CPU/GPU on controller per screen |
| 1 — Server render | Node.js renderer on controller, pushes pre-rendered frames | `screen` (raw_rgb, jpeg) | ~10–50ms render + transport | CPU on controller, ~50–200 MB per renderer |
| 2 — Native render | Controller pushes widget definitions, device renders locally | `screen` (widget_def) | <1ms definition push, device renders | Minimal on controller; CPU/flash on device |

The routing graph models this honestly:

- **Tier 0/1**: The controller has a rendering device (software renderer or
  Node.js process) with a video source port. A link connects it to the screen's
  sink port. The rendering device has resource costs on the controller (§2.7).

- **Tier 2**: The controller sends widget definitions (tiny JSON payloads) to
  the device. The device does its own rendering. The link carries `screen`
  format with `encoding: widget_def`. Resource cost on the controller is
  negligible; the device's capacity determines what it can render.

**Compound screen devices**: A Stream Deck is both a control surface (buttons)
and a screen (LCD per key or full LCD panel). It's modelled as a compound
device with:
- Control source ports (button presses, touch events)
- Screen sink ports (key images, full-screen content)
- Possibly separate ports per key region or a single port for the whole panel

**Connection types**:

```yaml
ConnectionInfo:
  transport: string             # "usb", "serial", "spi", "i2c", "wifi", "bluetooth", "network"
  bus_bandwidth_bps: uint64?    # connection bandwidth limit
  shared_bus: bool              # is this connection shared with other devices?
  latency_ms: float?            # connection latency
```

**Screen capacity** (what the screen can handle):

| Screen type | Typical resolution | Max fps | Connection | Bottleneck |
|------------|-------------------|---------|------------|------------|
| Stream Deck MK.2 (15 key) | 72×72 per key | 15 | USB HID | USB HID report size |
| Stream Deck XL (32 key) | 96×96 per key | 15 | USB HID | USB HID report size |
| Stream Deck + (full LCD) | 800×100 touch strip | 30 | USB | USB bandwidth |
| Corsair iCUE screen | 480×480 | 30 | USB | USB bandwidth |
| SSD1306 OLED | 128×64 mono | 60 | I2C/SPI | Bus speed (I2C: 400 kHz) |
| Waveshare e-ink | 400×300 | 0.2 | SPI | Panel refresh (~5s) |
| LED matrix (HUB75) | 64×32 | 60 | GPIO/HUB75 | GPIO timing |
| Browser widget | Arbitrary | 60 | WebSocket | Network + browser render |
| ESP32 + TFT | 320×240 | 30 | WiFi/BLE | Wireless bandwidth |

#### RGB endpoints

An RGB endpoint is any device that accepts color data for visual output via
LEDs. RGB endpoints are data sinks.

```yaml
RgbEndpoint:
  led_count: uint               # total individually addressable LEDs
  topology: RgbTopology         # how LEDs are arranged
  max_framerate: float          # maximum refresh rate
  color_model: string           # "rgb", "rgbw", "ww" (warm/cool white)
  color_depth: uint             # bits per channel
  power_draw_w: float?          # maximum power at full white
  protocol: string              # "ws2812", "sk6812", "apa102", "ddp", "artnet",
                                # "e131", "wled_json", "openrgb", "vendor_usb"
  connection: ConnectionInfo
  controller_type: string?      # "wled", "openrgb", "vendor", "direct_gpio"
```

**RgbTopology**:

```yaml
RgbTopology:
  type: string                  # "strip", "matrix", "zones", "per_key", "ring", "custom"
  dimensions: Dimensions?       # for matrix: { w, h }. For strip: { length_mm }
  zone_count: uint?             # for zone-based (e.g., motherboard RGB headers)
  zone_names: string[]?         # human-readable zone labels
  spatial_layout: SpatialLed[]? # per-LED positions in device-local coordinates (mm)
```

**Resource costs of RGB**: The bandwidth is small (§4.4), but the rendering cost
can be significant when running spatial effects across many devices:

| Scenario | LED count | Controller CPU | Notes |
|----------|----------|----------------|-------|
| Single keyboard per-key | 104 | <1% | Simple solid/gradient |
| Single WLED strip | 300 | <1% | Direct DDP output |
| Full room spatial effect | 1500+ | 3–8% | World-space effect function evaluated per LED per frame |
| Reactive key effects | 104 | 1–3% | Per-keypress effect computation |

The RGB compositor (layered rendering engine) is modelled as a processing
device in the graph with resource costs on the controller.

#### Control surfaces

A control surface is any device that sends control input to the controller
and optionally receives visual/haptic feedback. Control surfaces have both
source ports (input) and sink ports (feedback).

```yaml
ControlSurfaceEndpoint:
  inputs: ControlInputSet       # buttons, faders, encoders, axes (see §4.1)
  outputs: ControlOutputSet?    # LEDs, displays, motors (see §4.1)
  connection: ConnectionInfo
  protocol: string              # native protocol ("midi", "osc", "hid", "serial", "streamdeck_hid")
  bidirectional: bool           # does the device accept feedback?
  pages: uint?                  # number of switchable pages/layers
  profiles: bool?               # does the device support on-device profiles?
```

**Control surface as compound device**: Many control surfaces are compound:

- **Stream Deck**: buttons (control source) + LCD keys (screen sinks) + touch
  strip (control source) + full-screen LCD (screen sink). 4+ ports.
- **X-Touch Mini**: encoders + buttons (control source) + LED rings + button
  LEDs (control sink via MIDI). 2 ports.
- **Gamepad**: axes + buttons + triggers (control source) + rumble motors
  (control sink). 2 ports.
- **ShuttlePRO v2**: jog wheel + shuttle ring + buttons (control source).
  1 port (no feedback).

The routing graph models each independently. The controller can send screen
data to a Stream Deck's LCD while simultaneously receiving button presses —
these are separate pipelines with separate resource costs.

**Control surface resource costs**:

| Device | Controller CPU | Memory | Notes |
|--------|---------------|--------|-------|
| MIDI surface (polling) | <0.5% | ~5 MB | Low-frequency events |
| Stream Deck (15 key, rendering) | 2–5% | ~80 MB | Per-key image rendering |
| Stream Deck + (full LCD) | 3–8% | ~120 MB | Full-screen compositing |
| Gamepad (polling at 250 Hz) | <0.5% | ~3 MB | Lightweight |
| OSC surface (network) | <0.5% | ~5 MB | Event-driven |
| Multiple surfaces (5 devices) | 5–15% | ~300 MB | Cumulative |

#### Endpoints in pipeline management

Endpoint devices participate in the same pipeline lifecycle as video/audio/HID:

**Warm pipelines for screens**: A Stream Deck's rendering pipeline can be kept
warm — the renderer process stays running, frame buffer allocated, widget state
cached. On scenario switch, only the content changes, not the pipeline. This
is important because Stream Deck image upload is relatively slow over USB HID.

**Activation time**: Different endpoints have different activation times:

| Endpoint | cold→warm | warm→active | Notes |
|----------|-----------|-------------|-------|
| WLED strip (UDP) | ~50ms | ~0ms | mDNS discovery + first frame |
| Stream Deck (USB) | 500–2000ms | <10ms | USB enumeration + device init |
| MIDI surface (USB) | 200–500ms | <10ms | ALSA/CoreMIDI device open |
| e-ink screen (SPI) | 100ms | ~5000ms | Fast init, slow panel refresh |
| OSC surface (network) | ~10ms | ~0ms | UDP socket, no handshake |
| Art-Net fixture (network) | ~10ms | ~0ms | UDP broadcast |

**Degradation**: When the controller is under resource pressure (§2.7),
endpoint pipelines are degraded before primary KVM pipelines:

1. Reduce screen refresh rate (30 fps → 10 fps → on-change-only)
2. Reduce RGB effect framerate (60 fps → 30 fps → 15 fps)
3. Simplify RGB effects (spatial → per-device solid)
4. Reduce screen rendering quality (JPEG quality, resolution)
5. Pause non-essential screens entirely (keep primary status screen)
6. HID/audio/video pipelines are never degraded to save endpoint resources

This degradation order is automatic — the router applies it based on resource
pressure and pipeline priority. Endpoint pipelines always have lower priority
than KVM pipelines unless explicitly overridden.

**Capacity limits**: An SBC node (Pi 5) might be able to drive 2 WLED strips
and render 1 Stream Deck screen simultaneously, but adding a third WLED strip
and a second Stream Deck would push it over capacity. The router knows this
from the device capacity model (§2.7) and will reject or degrade pipelines
that exceed the node's limits.

### 2.9 Additional Device Classes

Several device classes require more detail than a table row. This section
covers devices that are compound, have unusual port configurations, or
integrate with external systems.

#### Cameras

A camera is a video source device. Different camera types have different
discovery mechanisms, transport types, and control capabilities:

```yaml
CameraDevice:
  type: camera
  camera_type: string           # "v4l2", "rtsp", "onvif", "ndi", "virtual",
                                # "doorbell", "frigate", "mobile"
  ports:
    - id: video_out             # video source port
      direction: source
      media_type: video
    - id: audio_out             # audio source (if supported)
      direction: source
      media_type: audio
    - id: ptz_control           # PTZ control (ONVIF/NDI cameras)
      direction: sink
      media_type: control
    - id: audio_in              # two-way audio (doorbells, intercoms)
      direction: sink
      media_type: audio
    - id: events                # motion/person/object detection events
      direction: source
      media_type: data
  capabilities:
    ptz: bool                   # pan/tilt/zoom control
    two_way_audio: bool         # can receive audio (doorbell, intercom)
    local_detection: bool       # on-device AI detection
    privacy_zones: bool         # supports privacy masking
    night_vision: bool
    ir_illuminator: bool
```

Doorbell cameras are compound: video source + audio source + audio sink
(two-way) + event source (button press, motion) + control sink (unlock
command). The routing graph expresses all of these as separate ports.

Frigate-managed cameras are discovered via the Frigate API. Their video ports
may source from Frigate's re-stream (RTSP) rather than directly from the
camera, adding a hop but gaining Frigate's detection events.

#### Phones

A phone connected via USB or KDE Connect is a compound device with multiple
independent ports:

```yaml
PhoneDevice:
  type: phone
  vendor: string                # detected vendor (Samsung, Google, Apple, etc.)
  connection: string            # "usb", "kdeconnect", "bluetooth"
  ports:
    - id: audio_out             # phone speaker playback (UAC2 gadget)
      direction: sink
      media_type: audio
    - id: audio_in              # phone microphone capture (UAC2 gadget)
      direction: source
      media_type: audio
    - id: screen_mirror         # screen content (ADB/usbmuxd)
      direction: source
      media_type: video
    - id: tethering             # network via USB (CDC ECM/NCM)
      direction: source
      media_type: data
    - id: notifications         # notification stream (KDE Connect)
      direction: source
      media_type: data
    - id: clipboard             # shared clipboard (KDE Connect)
      direction: source | sink  # bidirectional
      media_type: data
    - id: media_control         # play/pause/next/prev (KDE Connect)
      direction: sink
      media_type: control
    - id: battery               # battery state
      direction: source
      media_type: data
  usb_pd: UsbPdState?           # if USB-PD capable: voltage, current, charging state
```

Not all ports are active simultaneously. USB audio and screen mirror may
conflict for USB bandwidth. The router respects the device's USB bus capacity
(§2.7) when deciding which ports to activate.

#### Actuators and motion devices

Actuators are controllable physical devices — monitor stands, sit/stand desks,
cranes, linear actuators. They are sink devices that accept position commands
and source devices that report current position.

```yaml
ActuatorDevice:
  type: actuator
  actuator_type: string         # "monitor_stand", "desk", "crane", "linear", "servo"
  connection: ConnectionInfo    # serial, BLE, HTTP, MQTT
  ports:
    - id: position_control      # accept position commands
      direction: sink
      media_type: control
    - id: position_report       # report current position
      direction: source
      media_type: data
  axes: ActuatorAxis[]          # controllable dimensions

ActuatorAxis:
  name: string                  # "height", "tilt", "pan", "extend"
  min: float                    # minimum position (mm or degrees)
  max: float                    # maximum position
  speed: float?                 # movement speed (mm/s or deg/s)
  presets: Preset[]?            # named positions ("standing", "sitting", "presentation")
```

Actuators affect the routing graph indirectly: when a desk reaches the
"standing" preset, a trigger can activate a different scenario which assembles
different pipelines. Actuator position also feeds the world layout (§ device
database / spatial RGB) — when a desk moves, all devices on it move in the
3D model.

#### Sensors

Sensors are source devices that produce data readings. They have data source
ports.

```yaml
SensorDevice:
  type: sensor
  sensor_type: string           # "temperature", "current", "humidity", "motion",
                                # "door_contact", "tamper", "power", "light"
  connection: ConnectionInfo    # I2C, SPI, GPIO, USB, MQTT, network
  ports:
    - id: reading               # sensor data stream
      direction: source
      media_type: data
  data_schema: DataSchema       # what fields this sensor produces (§4.1)
  sample_rate_hz: float         # how often it reads
  alert_thresholds: Threshold[]? # configured alert levels
```

Sensors feed the trigger engine, SIEM, compliance evidence, and screen widgets.
They don't participate in media pipelines (no video/audio/HID), but they are
graph devices with data ports, resource costs, and version information.

#### Audio processors

Audio processors are inline processing devices — they sit in an audio pipeline
between source and sink, transforming the audio. They have both audio sink
(input) and audio source (output) ports.

```yaml
AudioProcessorDevice:
  type: audio_processor
  processor_type: string        # "room_correction", "eq", "compressor", "gate",
                                # "ducker", "voice_detector", "noise_monitor",
                                # "transcription"
  ports:
    - id: audio_in
      direction: sink
      media_type: audio
    - id: audio_out
      direction: source
      media_type: audio
    - id: sidechain_in          # optional sidechain input (for ducking, etc.)
      direction: sink
      media_type: audio
    - id: data_out              # analysis output (voice detection events, levels, transcript)
      direction: source
      media_type: data
  latency_ms: float             # processing latency added to pipeline
  resource_cost: ResourceCost   # CPU/memory on the host device
```

The router models audio processors as hops in the pipeline. A room correction
filter adds ~5ms latency and ~3% CPU. The intent's latency budget must
accommodate this. If `fidelity_audio` intent has a 10ms budget and the pipeline
already uses 6ms for transport, the room correction filter (5ms) would bust
the budget — the router either skips it or the user relaxes the constraint.

#### VM hosts and virtual targets

A VM host (QEMU/KVM, Proxmox, libvirt) is a device that contains virtual
machine targets. Each VM is a target device with special ports:

```yaml
VmHostDevice:
  type: vm_host
  hypervisor: string            # "qemu", "proxmox", "libvirt", "hyper-v"
  connection: string            # "qmp", "libvirt", "dbus", "proxmox_api"
  vms: VmTarget[]               # discovered VMs

VmTarget:
  type: target
  vm_name: string
  vm_id: string                 # hypervisor-specific ID
  ports:
    - id: hid_in                # HID injection (QMP input-send-event)
      direction: sink
      media_type: hid
    - id: display_out           # display output (QMP screendump, D-Bus display, Looking Glass)
      direction: source
      media_type: video
    - id: power_control         # power management (start, stop, reset, pause)
      direction: sink
      media_type: control
    - id: serial_console        # serial/console output
      direction: source
      media_type: data
    - id: agent_channel         # virtio-serial to guest agent
      direction: source | sink
      media_type: data
  display_transport: string     # "qmp_screendump", "dbus_display", "looking_glass", "vnc", "spice"
```

Looking Glass deserves special note: it uses IVSHMEM (shared memory between
host and VM) for zero-copy frame sharing. This is a transport plugin with
extraordinary bandwidth (limited only by memory bus speed) and near-zero
latency. The router should prefer it over VNC/screendump when available.

#### Displays with DDC/CI

Full-size displays (monitors, projectors) have control capabilities beyond
just being a video sink:

```yaml
DisplayDevice:
  type: display
  ports:
    - id: video_in              # HDMI/DP input (video sink)
      direction: sink
      media_type: video
    - id: ddc_control           # DDC/CI control port
      direction: sink
      media_type: control
  ddc_capabilities:
    brightness: bool            # adjustable brightness (0–100)
    contrast: bool              # adjustable contrast
    input_select: bool          # can switch HDMI/DP inputs
    power: bool                 # can power on/off/standby
    volume: bool                # built-in speaker volume
```

DDC/CI input switching makes a monitor behave like a simple switch (§2.5) —
the router can command a monitor to switch its input as part of pipeline
activation. This is modelled as a controllable device with `confirmed` or
`write_only` feedback depending on the monitor's DDC/CI implementation.

#### Managed services

Ecosystem services (Frigate, Jellyfin, Immich, Home Assistant, Vaultwarden,
Audiobookshelf) are devices in the graph with API ports:

```yaml
ServiceDevice:
  type: service
  service_type: string          # "frigate", "jellyfin", "immich", "homeassistant",
                                # "vaultwarden", "audiobookshelf"
  connection: ConnectionInfo    # typically HTTP/API on localhost or LAN
  ports:
    - id: api                   # REST/WebSocket API
      direction: source | sink
      media_type: data
    - id: events                # event stream (webhooks, MQTT, WebSocket)
      direction: source
      media_type: data
    - id: media_out             # media content (video streams, audio, photos)
      direction: source
      media_type: video | audio | data
  health: ServiceHealth         # running, degraded, stopped, unreachable
  managed: bool                 # is Ozma responsible for this service's lifecycle?
  container: ContainerInfo?     # if running in a container: image, state, resources
```

Services don't participate in real-time media pipelines (they're not KVM
components), but they are part of the graph because:
- They consume device resources (§2.7) — a Frigate instance uses GPU and CPU
- They are versioned and updatable through the mesh (§14)
- They produce events that feed triggers and SIEM
- Their health affects system health reporting
- The controller may manage their lifecycle (start, stop, update, backup)

#### Notification and metrics sinks

Output-only devices that consume events, alerts, or metrics:

```yaml
NotificationSinkDevice:
  type: notification_sink
  sink_type: string             # "webhook", "slack", "discord", "email",
                                # "pushover", "ntfy", "telegram"
  connection: ConnectionInfo    # HTTP, SMTP, etc.
  ports:
    - id: events_in
      direction: sink
      media_type: data

MetricsSinkDevice:
  type: metrics_sink
  sink_type: string             # "prometheus", "datadog", "influxdb", "syslog"
  connection: ConnectionInfo
  ports:
    - id: metrics_in
      direction: sink
      media_type: data
  format: string                # "prometheus_exposition", "statsd", "otlp", "syslog_rfc5424"
```

These are data sinks. They appear in the graph so the router can track their
health, resource usage, and connectivity — but they don't participate in
media pipeline routing.

#### Media receivers and sources

A media receiver is a software endpoint that receives audio (and sometimes
video) from an external streaming service and produces it locally as a
PipeWire source. A media source is a controllable content library that
can be directed to play through specific outputs.

```yaml
MediaReceiverDevice:
  type: media_receiver
  receiver_type: string         # "spotify_connect", "airplay", "chromecast",
                                # "dlna_renderer", "bluetooth_a2dp", "roc_receiver"
  ports:
    - id: audio_out             # decoded audio into PipeWire
      direction: source
      media_type: audio
    - id: video_out             # video output (Chromecast, AirPlay video)
      direction: source
      media_type: video
    - id: metadata              # track info, album art, playback state
      direction: source
      media_type: data
    - id: transport_control     # play/pause/skip/seek commands from the sender
      direction: sink
      media_type: control
  discovery: DiscoveryConfig    # how this receiver advertises itself
  playback_state: PlaybackState # what's currently playing

MediaSourceDevice:
  type: media_source
  source_type: string           # "jellyfin", "plex", "tidal_connect", "local_library",
                                # "youtube_music", "subsonic", "navidrome"
  ports:
    - id: audio_out             # audio stream (if controller decodes)
      direction: source
      media_type: audio
    - id: video_out             # video stream (if controller decodes)
      direction: source
      media_type: video
    - id: api_control           # library browsing, queue management
      direction: sink
      media_type: control
    - id: metadata              # now playing, queue, library info
      direction: source
      media_type: data
  connection: ConnectionInfo    # API endpoint
  can_cast_to: string[]?        # devices this source can direct playback to
                                # (Spotify can cast to any Spotify Connect device,
                                #  Jellyfin can cast to DLNA renderers, etc.)

DiscoveryConfig:
  protocol: string              # "mdns", "upnp_ssdp", "bluetooth"
  service_type: string?         # "_spotify-connect._tcp", "_raop._tcp",
                                # "_googlecast._tcp", "urn:schemas-upnp-org:device:MediaRenderer:1"
  instance_name: string?        # advertised name ("Living Room Speakers")

PlaybackState:
  state: string                 # "playing", "paused", "stopped", "buffering"
  track: TrackInfo?
  position_ms: uint?
  duration_ms: uint?
  volume: float?                # 0.0–1.0 (service-level volume, independent of Ozma volume)
  source: string?               # who's sending ("matt's phone", "desktop app")

TrackInfo:
  title: string?
  artist: string?
  album: string?
  art_url: string?              # album art URL (for screen endpoints, dashboard)
  genre: string?
  codec: string?                # source codec ("ogg_vorbis", "aac", "flac", "mqa")
  sample_rate: uint?            # source sample rate (if known)
  bit_depth: uint?              # source bit depth (if known)
  lossy: bool?                  # is the source lossy?
```

**Why media receivers are in the routing graph**:

A Spotify Connect receiver isn't just a service — it's an audio source in
the routing graph with a PipeWire port. The router needs to know about it
because:

1. **It produces audio that enters the mix bus.** When Spotify plays, its
   audio competes for the desk speakers with KVM node audio. The mix bus
   (§2.13) handles summing, but the router needs to know the source exists.

2. **It has format properties.** Spotify outputs Ogg Vorbis 320kbps (lossy).
   Tidal can output FLAC (lossless) or MQA. The `fidelity_audio` intent
   would reject a Spotify source but accept Tidal FLAC — this is format
   negotiation.

3. **It can be routed to multiple outputs.** Spotify playing on the
   controller can be sent to desk speakers + AirPlay living room +
   VBAN to kitchen — this is fan-out from one source to multiple sinks
   via audio output targets.

4. **Metadata feeds screen endpoints.** Track info and album art from
   `metadata` port → Stream Deck key images, OLED status display,
   dashboard now-playing widget. This is a `data` pipeline from the
   media receiver to screen sinks.

5. **Intent bindings react to playback state.** "When Spotify starts
   playing, switch to music intent (lower KVM audio, enable room
   correction, set RGB to ambient mode)" — the `PlaybackState` is a
   condition source for intent bindings (§8.7).

**Casting model**:

Some services can direct playback to a specific device — Spotify Connect,
AirPlay, Chromecast. This is a **control path** (§2.12) operation: the
controller tells the service "play through this receiver". The audio then
appears at that receiver's PipeWire port. The `can_cast_to` field on
MediaSourceDevice indicates which receivers a source can target.

This is distinct from Ozma's own audio routing. Spotify casting routes
within Spotify's infrastructure; Ozma routing happens after the audio
reaches PipeWire. Both can coexist — Spotify casts to the controller's
Spotify Connect receiver, then Ozma routes the PipeWire output to
multiple speakers via its own transport plugins.

#### Network switches and routers

A managed network switch is a compound device with multiple Ethernet ports
connected by a switching fabric. It's directly analogous to an HDMI matrix
switch (§2.5) — but for network traffic instead of video:

```yaml
NetworkSwitchDevice:
  type: network_switch
  ports:
    - id: port_1                # each physical port
      direction: source | sink  # bidirectional
      media_type: data
      capabilities:
        speed_mbps: [100, 1000, 2500]  # auto-negotiated
        poe: PoePowerState?     # if this port delivers PoE
    # ... port_2 through port_N
    - id: sfp_1                 # SFP/SFP+ cages
      direction: source | sink
      media_type: data
      capabilities:
        speed_mbps: [1000, 10000]
    - id: management            # management interface (SSH, HTTP, SNMP)
      direction: sink
      media_type: control
  internal_topology:
    fabric_bandwidth_gbps: float  # total switching backplane capacity
    blocking: string              # "non_blocking", "blocking_2:1", etc.
    vlan_support: bool
    igmp_snooping: bool
    link_aggregation: bool
  controllability:
    state_readable: bool          # can we query port status, VLAN config, PoE state?
    state_writable: bool          # can we configure VLANs, enable/disable ports?
    feedback: confirmed | write_only | manual
    control_interface: string     # "snmp", "ssh", "http_api", "unifi", "mikrotik", "openwrt"
```

A **router** extends this with WAN interfaces and gateway functionality:

```yaml
RouterDevice:
  type: router
  ports:
    - id: wan                   # WAN interface (different characteristics from LAN)
      direction: source | sink
      media_type: data
      capabilities:
        speed_mbps: [100, 1000]
        wan_type: string        # "ethernet", "fibre", "dsl", "cable", "cellular"
    - id: lan_1                 # LAN ports
      direction: source | sink
      media_type: data
    # ...
    - id: wifi_2g               # integrated WiFi (if present)
      direction: source | sink
      media_type: data
      capabilities:
        wifi_standard: "wifi6"
        bands: ["2.4ghz"]
    - id: wifi_5g
      direction: source | sink
      media_type: data
      capabilities:
        wifi_standard: "wifi6"
        bands: ["5ghz"]
  gateway: GatewayInfo?

GatewayInfo:
  nat: bool                     # performs NAT
  firewall: bool                # has firewall rules
  dhcp_server: bool
  dns_server: bool
  vpn: string[]?                # VPN types supported ("wireguard", "openvpn", "ipsec")
  upnp: bool?
  wan_ip: string?               # public IP (if known)
  wan_latency_ms: float?        # measured latency to upstream
```

Network switches and routers matter to the routing protocol because:
- Their **port speeds and PoE budgets** constrain what devices can connect
  and at what bandwidth
- Their **backplane capacity** determines whether the switch is a bottleneck
  (a cheap gigabit switch with a 2 Gbps backplane is blocking)
- Their **VLAN configuration** affects which devices can talk to each other
  (IoT VLAN isolation)
- Their **PoE power budget** is a power model concern (§2.10) — a PoE
  switch has a total power budget shared across all PoE ports
- The WAN interface has fundamentally different characteristics (latency,
  jitter, bandwidth) from LAN interfaces — the transport characteristics
  table (§6.1) depends on knowing whether traffic crosses the WAN

#### Media sessions on target machines

A target machine may have multiple media players running simultaneously —
Spotify playing music, YouTube paused in a browser tab, a game with its own
audio. The OS mixes all of these into a single system audio output, which
the node captures as one audio stream. But the **desktop agent** inside the
OS can observe each media session individually via platform APIs.

Media sessions on a target are modelled as child devices of the target,
reported by the desktop agent:

```yaml
MediaSessionDevice:
  type: media_receiver          # same type as controller-side receivers
  session_source: string        # "agent" — discovered by desktop agent, not by controller
  host_device: DeviceRef        # the target machine this session is on
  process: ProcessInfo          # which application
  ports:
    - id: audio_out             # this session's audio stream (if separable)
      direction: source
      media_type: audio
    - id: metadata              # track info, playback state
      direction: source
      media_type: data
    - id: transport_control     # play/pause/skip (via MPRIS2/SMTC)
      direction: sink
      media_type: control
  playback_state: PlaybackState # current state of this specific session
  audio_separable: bool         # can this session's audio be captured independently?

ProcessInfo:
  name: string                  # "spotify", "chrome", "vlc", "firefox"
  pid: uint?
  window_title: string?         # "Spotify - Bohemian Rhapsody", "YouTube - Some Video"
  app_id: string?               # "com.spotify.Client", "org.mozilla.firefox"
```

**Example — desktop with Spotify playing and YouTube paused**:

```yaml
# The target machine
- type: target
  id: "gaming-pc"
  ports:
    - id: system_audio           # mixed system audio (what the node captures)
      direction: source
      media_type: audio
      # This is Spotify + YouTube + system sounds, all mixed by the OS
  media_sessions:
    - type: media_receiver
      id: "gaming-pc/spotify"
      process: { name: "spotify", app_id: "com.spotify.Client" }
      playback_state:
        state: playing
        track: { title: "Bohemian Rhapsody", artist: "Queen",
                 codec: "flac", sample_rate: 44100, bit_depth: 16, lossy: false }
        volume: 0.8
      audio_separable: true      # PipeWire can isolate this stream
      # On Linux: PipeWire sees Spotify as a separate stream node
      # On Windows: WASAPI can capture per-app with process loopback

    - type: media_receiver
      id: "gaming-pc/chrome-youtube"
      process: { name: "chrome", window_title: "YouTube - Some Video" }
      playback_state:
        state: paused
        track: { title: "Some Video", codec: "opus", lossy: true }
        volume: 1.0
      audio_separable: true
```

**Audio separability**:

The `audio_separable` field indicates whether this session's audio can be
captured independently from the system mix:

| Platform | Per-app audio capture | How |
|----------|----------------------|-----|
| Linux (PipeWire) | Yes | Each app is a PipeWire stream node; agent can capture individually |
| Linux (PulseAudio) | Yes | `pactl` per-source-output capture |
| Windows 10+ | Yes | WASAPI process loopback (`AUDCLNT_PROCESS_LOOPBACK_MODE`) |
| macOS | Partial | Requires virtual audio driver (BlackHole/Loopback); per-app not native |

When audio is separable, the agent can capture individual streams and send
them to the controller as separate VBAN/Opus channels. The controller then
has per-source routing control — Spotify audio to the desk speakers at full
quality, YouTube audio muted, game audio ducked. This is a **per-application
mix bus** on the target machine, bridged to the controller via the agent.

When audio is NOT separable (no agent, or platform doesn't support it), the
controller receives mixed system audio as a single stream. Media session
metadata is still available (the agent reports playback state even if it
can't separate audio), so intent bindings and screen metadata still work —
you just can't route individual apps.

**How this affects the routing graph**:

1. **Without agent (node-only capture)**: One audio source port on the target.
   One `system_audio` stream. No per-app control. This is the basic KVM path.

2. **With agent, non-separable**: Same one audio source port, but media
   session metadata is available as data ports. Intent bindings can react
   to "Spotify is playing" even though audio can't be separated.

3. **With agent, separable audio**: Multiple audio source ports on the target,
   one per separable session. Each enters the routing graph independently.
   The controller's mix bus handles per-source volume/mute/routing. Full
   control.

**Intent binding examples**:

```yaml
# Duck all other audio when a video call starts
- conditions:
    - { source: media_session, field: process.name, op: in,
        value: ["zoom", "teams", "meet", "slack"] }
    - { source: media_session, field: playback_state.state, op: eq, value: "playing" }
  actions:
    - { type: "mix_bus.duck", target: "all_except_source", amount_db: -20 }

# Show now-playing on Stream Deck from whichever app is actively playing
- conditions:
    - { source: media_session, field: playback_state.state, op: eq, value: "playing" }
  actions:
    - { type: "screen.show_metadata", target: "streamdeck-key-5",
        data: "playback_state.track" }
```

#### Macro and synthetic input sources

The macro system and automation engine produce synthetic HID input. They are
modelled as virtual source devices:

```yaml
MacroSourceDevice:
  type: virtual
  virtual_type: string          # "macro_player", "automation_engine", "paste_typing"
  ports:
    - id: hid_out               # synthetic HID reports
      direction: source
      media_type: hid
    - id: control_in            # trigger/start/stop commands
      direction: sink
      media_type: control
```

When a macro plays, it produces HID reports that enter the routing graph at
a virtual source port. The router delivers them through the same pipeline as
physical keyboard input — the target device doesn't know the difference.

#### Clipboard

Cross-machine clipboard is a bidirectional data stream:

```yaml
ClipboardDevice:
  type: virtual
  virtual_type: string          # "clipboard_ring"
  ports:
    - id: clipboard             # clipboard data (text, image, file references)
      direction: source | sink
      media_type: data
  data_schema:
    fields:
      - { key: "content_type", type: "enum", enum_values: ["text", "image", "html", "file_ref"] }
      - { key: "content", type: "string" }
      - { key: "source_machine", type: "string" }
      - { key: "timestamp", type: "timestamp" }
```

### 2.10 Power Model

Power is a first-class concern in the routing graph. Every device consumes
power, many devices deliver power to other devices, and the available power
constrains what the system can do. Running 300 RGB LEDs at full white on a
USB port rated for 500mA will brown out the node. The routing protocol must
model power delivery, consumption, measurement, and pressure so the router
can make safe decisions.

#### Voltage rails

Power flows through **voltage rails** — named power paths at a specific
voltage. A device may have multiple rails (a PC has 3.3V, 5V, 12V). Each
rail has a capacity and a current state:

```yaml
VoltageRail:
  id: string                    # rail identifier ("5v_usb", "12v_pcie", "3v3_gpio", "48v_poe")
  nominal_voltage_v: float      # expected voltage (5.0, 12.0, 3.3, 48.0)
  voltage_range_v:              # acceptable operating range
    min: float                  # below this: under-voltage, device may malfunction
    warn_low: float             # below this: warn, approaching limit
    nominal: float              # expected voltage
    warn_high: float            # above this: warn, over-voltage
    max: float                  # above this: over-voltage, damage risk
  current_capacity_ma: float    # maximum deliverable current on this rail
  current_used_ma: float        # current draw (measured or inferred)
  current_available_ma: float   # capacity - used
  measured_voltage_v: float?    # actual measured voltage (if available)
  power_w: float?               # computed: voltage × current (for reporting)
  quality: InfoQuality          # how we know these numbers
  source: PowerSource           # where this rail's power comes from
```

**PowerSource** — where a rail gets its power:

```yaml
PowerSource:
  type: string                  # "usb_host", "usb_pd", "poe", "external_psu",
                                # "gpio_header", "battery", "barrel_jack",
                                # "pcie_slot", "sata_power", "molex"
  upstream_device: DeviceRef?   # which device supplies this power
  upstream_rail: string?        # which rail on the upstream device
  negotiated: bool              # was this power level negotiated? (USB PD, PoE)
  negotiation_state: PowerNegotiationState?  # if negotiated: current state
```

#### Voltage as a measurement proxy

Many devices cannot directly measure current draw. But they can measure
voltage, and voltage drop under load reveals current draw:

```yaml
VoltageMeasurement:
  rail_id: string               # which rail
  voltage_v: float              # measured voltage
  expected_v: float             # nominal voltage for this rail
  drop_v: float                 # expected - measured
  inferred_current_ma: float?   # estimated from voltage drop + known rail impedance
  inference_quality: InfoQuality # always lower than direct current measurement
  sensor: string?               # what's measuring ("ina219", "adc_ch3", "sysfs")
  timestamp: timestamp
```

A 5V USB rail measuring 4.72V tells you the combined load is drawing enough
current to cause a 0.28V drop across the cable and connector resistance. If
you know the cable resistance (~0.5Ω for a typical 0.5m USB cable), you can
estimate ~560mA. This is `measured` quality for the voltage, but only
`inferred` quality (a new sub-level of `measured`) for the current:

```yaml
InfoQuality: enum
  user
  measured
  inferred                      # derived from measured data + known/assumed parameters
  reported
  commanded
  spec
  assumed
```

**Trust ordering**: `user > measured > inferred > reported > commanded > spec > assumed`

The `inferred` level sits between `measured` and `reported` — it's based on
real measurements but requires assumptions (cable resistance, connector
quality) that may be wrong.

#### Power budgets on ports and links

Every port and link that carries power has a power budget:

```yaml
PortPowerBudget:
  delivers_power: bool          # does this port supply power to connected devices?
  consumes_power: bool          # does this port draw power from the connection?
  rail: VoltageRail?            # the voltage rail this port connects to
  max_current_ma: float?        # maximum current this specific port can deliver/draw
  current_draw_ma: float?       # current draw of the device on this port
  quality: InfoQuality
```

This applies to:

| Port type | Delivers | Consumes | Typical budget |
|-----------|----------|----------|---------------|
| USB-A host port | Yes | No | 500mA (USB2), 900mA (USB3) |
| USB-C host port | Yes | No | 1500mA (default), 3000mA (USB-C current) |
| USB-C with PD | Yes | No | Up to 5A @ 5–48V (negotiated) |
| USB gadget port | No | Yes | Declared via `max_power_ma` in ConfigFS |
| PoE port | Yes | No | 15.4W (af), 30W (at), 60W (bt Type 3), 90W (bt Type 4) |
| GPIO pin | Yes/No | Yes/No | 2–16mA per pin (SoC dependent) |
| GPIO power header | Yes | No | Total limited by regulator (e.g., 300mA on 3.3V) |
| PCIe slot | Yes | No | 75W (x16), 25W (x1) |
| SATA power | Yes | No | 4.5A @ 5V, 4.5A @ 12V |
| Barrel jack | Yes | No | PSU rating |
| LED data pin | No | No | Signal only — power is separate |

#### USB power in detail

USB power is the most complex case because it involves negotiation, multiple
standards, and widespread non-compliance:

```yaml
UsbPowerState:
  standard: string              # "usb2", "usb3", "usb_c_default", "usb_c_1.5a",
                                # "usb_c_3a", "usb_pd"
  negotiated_voltage_v: float   # actual negotiated voltage (5V default, up to 48V PD)
  negotiated_current_ma: float  # actual negotiated current limit
  pd_state: UsbPdState?         # if USB PD: full negotiation state

UsbPdState:
  source_pdos: PdObject[]       # what the source offered (Power Data Objects)
  selected_pdo: PdObject        # which PDO was selected
  active_voltage_v: float       # current voltage
  active_current_ma: float      # current limit
  pps: bool                     # Programmable Power Supply mode active?
  pps_range: { min_v: float, max_v: float, max_ma: float }?

PdObject:
  type: string                  # "fixed", "variable", "pps", "avs"
  voltage_v: float              # for fixed: exact voltage. For variable: max voltage.
  min_voltage_v: float?         # for variable/PPS: minimum voltage
  current_ma: float             # maximum current at this voltage
  power_w: float                # computed: voltage × current
```

#### PoE power

```yaml
PoePowerState:
  standard: string              # "802.3af", "802.3at", "802.3bt_type3", "802.3bt_type4",
                                # "passive_24v", "passive_48v"
  class: uint?                  # PoE class (0–8)
  allocated_w: float            # power allocated by the switch
  used_w: float?                # actual power draw (if switch reports it)
  voltage_v: float?             # measured voltage (typically 48V nominal)
```

#### Device power profile

Every device in the graph has a power profile describing what it consumes
and what it delivers:

```yaml
DevicePowerProfile:
  # What this device consumes
  consumption: PowerConsumption
  # What this device delivers to other devices
  delivery: PowerDelivery?
  # Battery (if applicable)
  battery: BatteryState?

PowerConsumption:
  idle_w: float?                # power draw when idle
  typical_w: float?             # power draw under typical load
  peak_w: float?                # maximum power draw (transient)
  per_function: FunctionPowerCost[]?  # breakdown by function
  source_rail: string           # which rail this device draws from
  quality: InfoQuality

FunctionPowerCost:
  function: string              # what function ("hid_gadget", "video_capture",
                                # "rgb_all_white", "rgb_typical", "wifi_active",
                                # "cpu_full_load", "encode_h264")
  current_ma: float             # additional current draw when this function is active
  voltage_v: float              # on which voltage rail
  quality: InfoQuality          # how we know this
  notes: string?                # e.g., "60mA per LED at full white, 20mA typical"

PowerDelivery:
  rails: VoltageRail[]          # voltage rails this device provides
  total_power_w: float?         # total deliverable power across all rails
  ports: PortPowerBudget[]      # per-port power budgets
```

**BatteryState** (phones, wireless peripherals, UPS, laptops):

```yaml
BatteryState:
  present: bool
  chemistry: string?            # "li_ion", "li_po", "nimh", "lead_acid"
  capacity_mah: uint?           # rated capacity
  current_percent: float        # 0–100
  current_voltage_v: float?     # current battery voltage
  charging: bool
  charge_rate_ma: float?        # current charge rate
  time_to_empty_min: float?     # estimated runtime
  time_to_full_min: float?      # estimated charge time
  health_percent: float?        # battery health (capacity vs design)
  cycles: uint?                 # charge cycle count
  quality: InfoQuality
```

#### RGB power — the biggest pressure point

RGB is the most common case where power pressure actually matters in
practice. A WS2812B LED draws up to 60mA at full white (20mA per channel),
but only ~20mA at typical use. This scales linearly:

| LED count | Full white (60mA/LED) | Typical (20mA/LED) | 5V rail current |
|----------|----------------------|--------------------|-----------------| 
| 30 | 1.8A / 9W | 0.6A / 3W | Within USB limits |
| 60 | 3.6A / 18W | 1.2A / 6W | Exceeds USB 3.0 (900mA) |
| 144 | 8.6A / 43W | 2.9A / 14.4W | Needs dedicated PSU |
| 300 | 18A / 90W | 6A / 30W | Needs beefy 5V PSU |

The router must know:
1. How many LEDs are on this strip (from device database, §15)
2. What power the current effect demands (from the RGB compositor)
3. What power is available on the rail feeding this strip
4. Whether the effect would exceed the power budget

**RGB power estimation**: The RGB compositor knows what colors it's rendering.
Full white = 60mA/LED. Pure red/green/blue = 20mA/LED. Black = ~1mA/LED
(quiescent). The compositor computes frame-by-frame power estimates:

```
estimated_current_ma = sum(
  led_current(r, g, b)  # per-channel: (channel_value / 255) × 20mA
  for each LED in the strip
)
```

This feeds into the power model as a `measured` quality current estimate
(based on known LED characteristics and the actual frame data).

**Power limiting in the RGB compositor**: When estimated power exceeds the
rail budget, the compositor can:
1. Scale global brightness to fit within budget (preferred — invisible to user)
2. Reduce color saturation toward black
3. Alert the user that the effect exceeds the available power

This is not the router's job — it's the compositor's. But the router needs
the power model to surface the pressure and to reject pipelines that would
overload a rail.

#### Power in the routing graph

Power adds a new dimension to the graph. Every device has:
- `power_profile` on the Device (what it consumes and delivers)
- `power_budget` on each Port that carries power
- `power_state` reflecting current measurements

The router uses this for:

**1. Pipeline feasibility**: Adding a pipeline through a device increases its
power draw. If the device's power source can't handle it, the pipeline is
rejected:

```
For each device in pipeline:
  for each rail the pipeline's functions draw from:
    if (rail.current_used_ma + pipeline.function_cost_ma) > rail.current_capacity_ma:
      reject pipeline  # would exceed rail capacity
```

**2. Warm pipeline power accounting**: Keeping pipelines warm costs power
(§2.6). The WarmCost already tracks this informally — now it's backed by
the power model. Three warm ffmpeg processes on a Pi 5 might collectively
draw 2W. The router checks this against the Pi's power supply headroom
before deciding to keep them warm.

**3. Power pressure alerts**: When any rail drops below `warn_low` voltage
or approaches current capacity, the router can:
- Degrade RGB effects (reduce brightness)
- Cool warm pipelines (reduce idle power)
- Alert the user ("USB port on Node 1 is near power limit")

**4. Power-aware device placement**: When the user adds a new device to the
graph (e.g., plugs a USB capture card into a node), the router checks whether
the node's USB power budget can support it. If the node is already running
an RGB strip on the same USB controller's 5V rail, it warns about the
combined draw.

#### Power discovery

| Source | Platform | What it provides | Quality |
|--------|----------|-----------------|---------|
| INA219 (I2C current sensor) | Any | Direct voltage + current measurement | `measured` |
| sysfs power supply class | Linux | Battery state, USB PD state, charger info | `reported` |
| USB descriptor `bMaxPower` | Any | Declared max current draw of USB device | `spec` |
| USB PD source capabilities | USB-C | Available PDOs from the power source | `reported` |
| PoE switch LLDP/CDP | Network | Allocated PoE power class | `reported` |
| Device database entry | Any | Rated power consumption per function | `spec` |
| Voltage measurement (ADC) | SBC | Rail voltage → inferred current | `inferred` |
| Smart PSU (PMBus/IPMI) | Server | Per-rail voltage, current, power | `measured` |
| UPS via NUT | Any | Input/output voltage, load %, battery state | `reported` |
| None (unknown device) | Any | USB class default (500mA USB2, 900mA USB3) | `assumed` |

#### Power in the device database (§15)

The `PowerSpec` block on device database entries:

```yaml
PowerSpec:
  input_voltage: VoltageRange?          # what voltage this device expects
  input_current_max_ma: float?          # maximum input current draw
  idle_power_w: float?                  # typical idle power
  peak_power_w: float?                  # maximum power (all functions active)
  power_source_options: string[]?       # ["usb", "barrel_5v", "poe", "battery"]
  per_function_power: FunctionPowerCost[]?  # breakdown by function
  efficiency_percent: float?            # PSU/regulator efficiency (for delivery devices)

  # For devices that deliver power:
  output_rails: OutputRail[]?

  # For batteries:
  battery_spec: BatterySpec?

  # For LED devices:
  led_power: LedPowerSpec?

VoltageRange:
  nominal: float                # expected input voltage
  min: float                    # minimum operating voltage
  max: float                    # maximum safe voltage

OutputRail:
  voltage_v: float              # output voltage
  max_current_ma: float         # maximum deliverable current
  regulation: string?           # "linear", "switching", "unregulated"
  shared_with: string[]?        # other outputs sharing this rail's capacity

BatterySpec:
  chemistry: string             # "li_ion", "li_po", "nimh", "lead_acid"
  capacity_mah: uint
  nominal_voltage_v: float
  charge_voltage_v: float?      # full charge voltage
  cutoff_voltage_v: float?      # discharge cutoff
  max_charge_rate_c: float?     # maximum charge rate (C-rate)

LedPowerSpec:
  type: string                  # "ws2812b", "sk6812", "apa102", "ws2815"
  per_led_max_ma: float         # max current per LED at full white (60mA for WS2812B)
  per_channel_max_ma: float     # max per color channel (20mA for WS2812B)
  quiescent_ma: float           # current per LED when displaying black (~1mA)
  voltage: float                # LED operating voltage (5V or 12V)
  recommended_psu_headroom: float  # recommended PSU headroom factor (1.2 = 20% over max)
```

#### Observability

```
GET /api/v1/routing/power                    # all power rails across all devices
GET /api/v1/routing/power/{device_id}        # power state for a specific device
GET /api/v1/routing/power/{device_id}/rails  # per-rail detail with measurements
GET /api/v1/routing/power/pressure           # devices with power pressure warnings
```

**Events**:

```
routing.power.rail_warning       # voltage or current approaching limits
routing.power.rail_critical      # voltage below min or current exceeding capacity
routing.power.budget_exceeded    # device power draw exceeds source capacity
routing.power.pd_negotiated      # USB PD negotiation completed (new voltage/current)
routing.power.battery_low        # battery below threshold
routing.power.rgb_power_limited  # RGB brightness scaled down due to power limit
```

### 2.11 Furniture and Physical Environment

Furniture — desks, chairs, shelves, racks, sofas, tables — is not a device
in the routing graph (it has no ports, no data flows through it). But it is
a first-class entity in the physical model because:

1. Devices are positioned relative to furniture ("keyboard on desk",
   "node under desk", "camera on shelf")
2. Furniture state affects routing (desk height → zone inference → intent
   binding, chair occupancy → presence detection)
3. Furniture defines the spatial structure that zones and the 3D scene are
   built from
4. Some furniture is motorised and controllable (sit/stand desks, monitor
   arms) — these are both furniture and actuator devices (§2.9)

```yaml
FurnitureEntity:
  id: string                    # unique identifier
  type: FurnitureType           # what kind of furniture
  name: string                  # human-readable ("Main desk", "Couch")
  device_db_id: string?         # device database entry (§15) for dimensions, model, etc.
  location: PhysicalLocation    # where this furniture is (§2.1)
  contains: ContainedEntity[]   # devices and other entities on/in this furniture
  state: FurnitureState?        # current physical state (height, occupancy, etc.)
  actuator_device: DeviceRef?   # if motorised: the actuator device in the routing graph

FurnitureType: enum
  desk                          # work surface (may be sit/stand)
  table                         # non-work table (dining, coffee, conference)
  chair                         # seating for one (office, gaming)
  sofa                          # multi-seat seating
  shelf                         # wall or freestanding shelf
  rack                          # server/network rack (19", 10")
  cabinet                       # closed storage
  stand                         # monitor stand, speaker stand, headphone stand
  mount                         # wall mount, arm mount, ceiling mount
  cart                          # mobile cart or trolley
  custom                        # anything else

ContainedEntity:
  entity_id: string             # device ID, furniture ID, or non-device entity ID
  entity_type: string           # "device", "furniture", "decoration"
  relationship: string          # from RelativeLocation relationships (§2.1)
  slot: string?                 # named slot (for racks: "U12", for desks: "left_side")
  offset: Position3d?           # offset from furniture origin
```

**FurnitureState** — dynamic physical state that some furniture reports:

```yaml
FurnitureState:
  height_mm: float?             # current height (sit/stand desks)
  height_preset: string?        # matched preset name ("sitting", "standing")
  tilt_deg: float?              # current tilt (monitor arms)
  occupied: bool?               # is someone sitting here? (pressure sensor, camera inference)
  position_preset: string?      # matched position preset
  quality: InfoQuality          # how we know this state
  source: string?               # "ble_sensor", "serial", "camera_inference", "manual"
```

**Furniture in zones**: Furniture naturally defines zones. A desk and its
devices form a `workstation` zone. A sofa and TV form a `media` zone. Zones
can be defined explicitly, but the system can also infer them from furniture
groupings — all devices whose `location.relative_to` points to the same desk
are in the same zone.

**Racks as containment**: A server rack is furniture that contains nodes,
switches, patch panels, UPS units, and PDUs. The rack is a `FurnitureEntity`
of type `rack`. Each rack unit is a named slot. Devices are placed in slots:

```yaml
# Server rack with nodes
id: "rack-1"
type: rack
name: "Server Rack"
device_db_id: "generic-42u-rack"
location:
  physical:
    space: "server_closet"
    zone: "server-zone"
    pos: { x: 0, y: 0, z: 0 }
contains:
  - { entity_id: "pdu-1", entity_type: "device", slot: "pdu-left" }
  - { entity_id: "ups-1", entity_type: "device", slot: "U1-U4" }
  - { entity_id: "switch-1", entity_type: "device", slot: "U5" }
  - { entity_id: "node-server-1", entity_type: "device", slot: "U8" }
  - { entity_id: "node-server-2", entity_type: "device", slot: "U9" }
  - { entity_id: "patch-panel", entity_type: "device", slot: "U6" }
```

**Rooms and sites**: Rooms are containers for zones and furniture. Sites are
containers for rooms. This hierarchy is optional — a single-desk home setup
doesn't need it. But a multi-room office or a school with multiple labs does:

```yaml
Site:
  id: string
  name: string                  # "Home", "Office HQ", "School Main Building"
  address: string?              # physical address
  timezone: string?             # IANA timezone
  spaces: Space[]

Space:
  id: string
  name: string                  # "Study", "Living Room", "Server Closet", "Lab 3"
  type: SpaceType               # room type
  floor: int?                   # floor number (0 = ground)
  dimensions_mm: Dimensions?    # room dimensions
  zones: SpatialZone[]          # zones within this space
  furniture: FurnitureEntity[]  # furniture in this space

SpaceType: enum
  office                        # private office
  open_plan                     # open plan workspace
  meeting_room                  # conference / meeting room
  lab                           # computer lab, workshop
  studio                        # recording / production studio
  living_room                   # residential living area
  bedroom                       # residential bedroom
  server_room                   # dedicated server/network room
  utility_room                  # storage, mechanical
  outdoor                       # patio, garden, yard
  classroom                     # teaching space
  common_area                   # kitchen, break room, hallway
  custom                        # anything else
```

**Physical environment is optional at every level**. A user with one desk
and two machines needs none of this — their devices just have bus-level
locations. A user who wants spatial RGB adds furniture positions. A business
with multiple rooms adds spaces and zones. An MSP managing 50 sites adds
the full hierarchy. Each level is independently useful and none is required.

### 2.12 Control Path

The routing graph has two planes: the **data plane** (how media flows — video,
audio, HID, RGB) and the **control plane** (how commands reach devices). The
spec models data plane routing in detail (§8), but control plane routing is
equally important — if the command can't reach the device, the device can't
be managed.

A control path describes how a command gets from the controller to a device.
Unlike data plane links which are always point-to-point between ports,
control paths may traverse intermediaries, use out-of-band channels, or
require specific physical connections.

```yaml
ControlPath:
  method: ControlMethod         # how commands are delivered
  reachability: Reachability    # can the controller currently reach this device?
  latency_ms: float?            # typical command round-trip time
  quality: InfoQuality

ControlMethod:
  type: string                  # transport type for control commands
  via: ControlVia               # what the command travels through
  protocol: string?             # application-level protocol
  address: string?              # how to reach it (IP, serial port, BLE address, etc.)
  credentials: string?          # credential reference (not the credential itself)
  fallback: ControlMethod?      # alternative control path if primary fails

ControlVia: enum
  direct                        # controller talks directly to device (IP, USB, local bus)
  proxy                         # controller talks through another device
  cloud                         # controller talks through a cloud service
  physical                      # requires physical human action (button press)
```

**Control path types**:

| Control type | Via | Example | Dependency |
|-------------|-----|---------|-----------|
| IP direct | `direct` | WLED HTTP API, SNMP | Network reachability |
| IP via cloud | `cloud` | UniFi Cloud Controller, Hue Cloud | Internet + vendor cloud |
| IP via local controller | `proxy` | UniFi via Cloud Key on LAN | Cloud Key must be running |
| Serial | `direct` | HDMI matrix via RS-232 | Serial cable + specific machine |
| Serial via node | `proxy` | HDMI matrix via USB-serial on Node 3 | Node 3 must be online |
| CEC | `direct` | TV via HDMI cable | Physically connected via HDMI |
| CEC via node | `proxy` | TV via HDMI, controlled from node that has the HDMI output | Node + HDMI cable |
| DDC/CI | `direct` | Monitor via display cable | Physically connected via DP/HDMI |
| DDC/CI via agent | `proxy` | Monitor DDC/CI via desktop agent on connected PC | Agent must be running |
| BLE | `direct` | Desk controller via BLE | BLE adapter in range |
| BLE via node | `proxy` | Desk controller via BLE on nearest node | Node with BLE + proximity |
| GPIO | `direct` | LoM relay on node GPIO | Specific node's GPIO |
| IR | `direct` | IR blaster attached to node | Node + IR hardware + line of sight |
| MQTT | `proxy` | IoT device via MQTT broker | Broker must be running |
| QMP | `direct` | QEMU VM via QMP socket | Hypervisor host + socket access |
| API via agent | `proxy` | Target machine feature via desktop agent | Agent running on target |
| Manual | `physical` | Physical button on device | Human present |

**Reachability**:

```yaml
Reachability:
  status: reachable | unreachable | degraded | unknown
  last_contact: timestamp?      # when we last successfully communicated
  failure_reason: string?       # if unreachable: why
  dependent_on: DeviceRef[]     # devices that must be online for this path to work
```

The `dependent_on` field is critical. It expresses the control dependency
chain:

```
Controller
  → can control: WLED strip (direct, IP)
  → can control: Node 3 (direct, IP)
  → can control: HDMI matrix (via Node 3, serial)
     dependent_on: [node-3]
  → can control: Monitor brightness (via Desktop Agent on Workstation A, DDC/CI)
     dependent_on: [agent-workstation-a]
  → can control: UniFi switch (via Cloud Key, API)
     dependent_on: [unifi-cloud-key]
  → can control: TV power (via Node 2, CEC over HDMI)
     dependent_on: [node-2]
```

If Node 3 goes offline, the controller loses control of the HDMI matrix —
even though the matrix itself is working. The router knows this because
`dependent_on: [node-3]` is explicit. This affects:

1. **Remediation** (§8.6): If the matrix needs switching as part of
   remediation, the router checks whether the control path is reachable
   before attempting it.

2. **Pipeline activation** (§8.5): If activating a pipeline requires
   switching the HDMI matrix, the router checks that Node 3 is online.
   If not, that pipeline is rejected (control path unreachable).

3. **Failover planning**: The router can pre-compute what would happen if
   each control proxy went offline. "If Node 3 dies, we lose control of
   the HDMI matrix and the serial console" — this feeds into the
   diagnostic/simulation API (§11.3).

4. **Redundant control paths**: Some devices have multiple control paths
   (e.g., a TV controllable via CEC from Node 2 OR via IP API directly).
   The `fallback` field on ControlMethod expresses this — if the primary
   path fails, try the fallback.

**Control path in the graph**:

Control paths are not modelled as links in the data plane graph — they don't
carry media, they don't have bandwidth or jitter. But they are part of the
graph's topology in the sense that they represent reachability and dependency.
The router considers them during pipeline assembly: a pipeline that requires
switching an externally-controlled device is only feasible if the control
path to that device is reachable.

**Cloud control path risks**:

When a device is controlled via a cloud service (`via: cloud`), the control
path depends on internet connectivity and the vendor's cloud availability.
The spec surfaces this as a dependency — the router can warn that "if your
internet goes down, you lose control of these devices" and recommend local
control alternatives where they exist. Devices with only cloud control paths
are flagged in the health dashboard.

**Events**:

```
control.path.reachable          # control path became reachable
control.path.unreachable        # control path lost (dependency down, network issue)
control.path.degraded           # control path latency increased significantly
control.path.fallback_activated # primary control path failed, using fallback
```

### 2.13 Audio Routing Model

The routing graph handles audio as pipelines between source and sink ports
(§2.4). For basic KVM audio (stereo from target machine to desk speakers),
this is sufficient. Professional audio requires additional primitives:
mix buses, monitor controllers, channel mapping, insert chains, metering,
and precision processing.

These are modelled as **virtual devices** in the routing graph — they don't
correspond to physical hardware, but they have ports, participate in
pipelines, consume resources, and contribute latency. On machines running
PipeWire, each virtual device maps to a PipeWire node (see §13.3).

#### Channel mapping

`AudioFormat` (§4.1) specifies channel count but not channel assignment. Pro
audio requires knowing which channel carries what signal:

```yaml
AudioFormat:
  # ... existing fields ...
  channel_layout: string?       # named layout (see table below)
  channel_map: string[]?        # explicit per-channel assignment, ITU-R BS.2051 labels

  # If channel_layout is set, channel_map is derived from it.
  # If channel_map is set, it overrides channel_layout.
  # If neither is set, channels are positional (ch0=left, ch1=right, etc.)
```

**Standard channel layouts**:

| Layout | Channels | Map |
|--------|----------|-----|
| `mono` | 1 | `["FC"]` |
| `stereo` | 2 | `["FL", "FR"]` |
| `stereo_lfe` | 3 | `["FL", "FR", "LFE"]` |
| `quad` | 4 | `["FL", "FR", "RL", "RR"]` |
| `5.1` | 6 | `["FL", "FR", "FC", "LFE", "SL", "SR"]` |
| `7.1` | 8 | `["FL", "FR", "FC", "LFE", "SL", "SR", "RL", "RR"]` |
| `7.1.4` | 12 | `["FL", "FR", "FC", "LFE", "SL", "SR", "RL", "RR", "TFL", "TFR", "TRL", "TRR"]` |
| `custom` | N | Explicit `channel_map` required |

Channel labels follow ITU-R BS.2051: `FL` (front left), `FR` (front right),
`FC` (front centre), `LFE` (low frequency effects), `SL`/`SR` (side left/right),
`RL`/`RR` (rear left/right), `TFL`/`TFR`/`TRL`/`TRR` (top/height channels).

When a link connects ports with different channel layouts, the converter
plugin (§6.4) performs channel remixing — upmix, downmix, or remap. The
format negotiation system (§4.3) handles this: if the source produces 7.1
and the sink accepts stereo, a downmix converter is inserted.

#### Mix bus

A mix bus is a virtual device that sums multiple audio inputs with per-input
gain, pan, and mute. It is the fundamental building block for monitor
controllers, headphone mixes, cue sends, and multi-source mixing.

```yaml
MixBusDevice:
  type: virtual
  virtual_type: "mix_bus"
  name: string                  # "Main Mix", "Headphone Mix A", "Cue Send 1"
  ports:
    - id: input_0               # one sink port per input source
      direction: sink
      media_type: audio
    - id: input_1
      direction: sink
      media_type: audio
    # ... up to N inputs
    - id: output                # one source port (summed output)
      direction: source
      media_type: audio
  config: MixBusConfig

MixBusConfig:
  output_format: AudioFormat    # output sample rate, bit depth, channels
  inputs: MixBusInput[]
  master_gain_db: float         # master output gain
  master_mute: bool

MixBusInput:
  source: PortRef               # which port feeds this input
  gain_db: float                # per-input gain (-inf to +12 dB)
  pan: float                    # -1.0 (full left) to +1.0 (full right), 0.0 = centre
  mute: bool
  solo: bool                    # solo-in-place (mute all non-soloed inputs)
  phase_invert: bool            # invert polarity
  channel_routing: ChannelRouting?  # custom channel routing for this input
```

A mix bus maps to a PipeWire node with N input ports and 1 output port
group. PipeWire's native port-level linking handles the per-channel
connections; gain/pan/mute are applied via PipeWire stream volume controls
or a `pw-filter-chain` summing node.

#### Monitor controller

A monitor controller is a compound virtual device that provides source
selection, speaker set switching, and monitoring utilities. It replaces
a $500–$2000 hardware monitor controller.

```yaml
MonitorControllerDevice:
  type: virtual
  virtual_type: "monitor_controller"
  ports:
    # Source inputs (any number of stereo/multichannel sources)
    - id: source_0              # e.g., "DAW Mix"
      direction: sink
      media_type: audio
    - id: source_1              # e.g., "Reference Player"
      direction: sink
      media_type: audio
    - id: source_2              # e.g., "Node Audio (target machine)"
      direction: sink
      media_type: audio
    # Speaker outputs (multiple speaker sets)
    - id: speakers_a            # e.g., "Main Monitors"
      direction: source
      media_type: audio
    - id: speakers_b            # e.g., "Secondary Monitors"
      direction: source
      media_type: audio
    - id: speakers_c            # e.g., "Headphones"
      direction: source
      media_type: audio
    - id: sub                   # subwoofer output (optional)
      direction: source
      media_type: audio
    # Talkback
    - id: talkback_mic          # engineer's microphone
      direction: sink
      media_type: audio
    - id: talkback_out          # routed to selected cue outputs
      direction: source
      media_type: audio
  config: MonitorControllerConfig

MonitorControllerConfig:
  active_source: uint           # which source input is selected (0-indexed)
  active_speakers: string[]     # which speaker sets are active (can be multiple)
  volume_db: float              # master volume
  dim: bool                     # reduce level by dim_amount_db
  dim_amount_db: float          # typically -20 dB
  mono: bool                    # sum to mono (for mono compatibility check)
  mute: bool                    # mute all outputs
  sub_enabled: bool             # route LFE to sub output
  sub_crossover_hz: float?      # crossover frequency (if bass management is active)
  talkback: TalkbackConfig

TalkbackConfig:
  mode: string                  # "momentary" (PTT), "latching", "auto" (voice-activated)
  destinations: string[]        # which outputs receive talkback ("cue_1", "cue_2")
  dim_on_talk: bool             # dim main monitors during talkback
  dim_amount_db: float          # how much to dim
  level_db: float               # talkback mic level
```

The monitor controller is a routing device — it's a managed switch (§2.5)
for audio, with additional processing (dim, mono, talkback). On PipeWire,
it maps to a combination of `pw-link` operations (source selection), volume
controls (dim/level), and a filter-chain node (mono sum, crossover).

#### Cue sends / aux sends

Cue sends allow per-source, per-destination level control — "send 50% of
source A to headphone mix B". These are modelled as mix buses with a
specific purpose:

```yaml
CueSendConfig:
  name: string                  # "Cue 1", "Headphone Mix A"
  mix_bus: MixBusDevice         # the underlying mix bus
  sends: CueSend[]              # per-source send levels
  output: PortRef               # where this cue mix goes (headphone amp, VBAN to node, etc.)

CueSend:
  source: PortRef               # which audio source
  send_level_db: float          # send level (-inf to +12 dB)
  pre_fader: bool               # true = level is independent of source's main fader
  pan: float                    # pan in the cue mix
  mute: bool
```

This maps to PipeWire as a separate mix bus node with per-input volumes.
Pre-fader sends tap the signal before the main mix bus gain stage.

#### Insert chain

An insert chain is an ordered sequence of audio processors (§2.9) applied
to a signal path. The order matters — EQ before compressor produces
different results than compressor before EQ.

```yaml
InsertChain:
  id: string
  name: string                  # "Main Bus Processing", "Vocal Chain"
  processors: InsertSlot[]      # ordered list of processors
  bypass: bool                  # bypass entire chain

InsertSlot:
  position: uint                # 0-indexed position in chain
  processor: DeviceRef          # audio_processor device
  bypass: bool                  # bypass this slot
  wet_dry: float?               # 0.0 = dry, 1.0 = full wet (for parallel processing)
```

On PipeWire, an insert chain is a sequence of `pw-filter-chain` nodes linked
in series. Each slot is a separate filter-chain node (allowing independent
bypass and parameter control). The pipeline model (§2.4) already supports
this — each processor is a hop with its own latency contribution.

#### Automatic latency compensation

When multiple audio paths have different processing chain lengths, their
latencies diverge. A source going through 3 processors arrives later than
one going through none. For time-aligned mixing, the shorter paths need
delay inserted to match the longest path:

```yaml
LatencyCompensation:
  mode: string                  # "auto", "manual", "disabled"
  reference_path: string?       # which pipeline is the reference (others compensate to match)
  per_path_delay: PathDelay[]   # computed or manual per-path delays

PathDelay:
  pipeline_id: string
  processing_latency_ms: float  # sum of processor latencies in this path
  compensation_delay_ms: float  # delay added to align with reference
  total_latency_ms: float       # processing + compensation = same for all paths
```

The router computes this automatically in `auto` mode: find the longest
processing chain latency across all paths feeding a mix bus, insert delay
on shorter paths to match. On PipeWire, delay insertion uses `pw-loopback
--delay`.

#### Metering

Metering is observation of audio levels at any point in the graph. The spec
defines metering types so that monitoring consumers know how to interpret
the data:

```yaml
MeteringPoint:
  port: PortRef                 # where in the graph to meter
  type: MeteringType            # what measurement algorithm
  channels: uint                # number of channels metered
  values: MeterValue[]          # current readings (per channel)
  update_rate_hz: float         # how often readings update

MeteringType: enum
  peak                          # instantaneous sample peak (dBFS)
  true_peak                     # inter-sample true peak per ITU-R BS.1770 (dBTP)
  rms                           # root mean square level (dBFS)
  vu                            # VU meter (300ms integration, +4 dBu = 0 VU)
  ppm                           # peak programme meter (5ms attack, configurable release)
  lufs_momentary                # ITU-R BS.1770 momentary loudness (400ms window)
  lufs_short_term               # ITU-R BS.1770 short-term (3s window)
  lufs_integrated               # ITU-R BS.1770 integrated loudness (programme duration)

MeterValue:
  channel: string               # channel label ("FL", "FR", "LFE", etc.)
  level: float                  # current level in the metering type's native unit
  peak_hold: float?             # peak hold value (highest level in decay window)
  clip: bool                    # true if signal has clipped (reached 0 dBFS)
```

Metering data feeds into the monitoring system (§11) and is available via
WebSocket events (`audio.meters`) and REST API. On PipeWire, metering reads
from `pw-dump` volume/peak data or from a dedicated analysis filter-chain.

#### Gain staging and headroom

Every audio hop in a pipeline has a gain contribution. The total gain through
the pipeline determines whether the signal clips or is too quiet:

```yaml
GainStage:
  hop: PipelineHopRef           # which hop in the pipeline
  input_level_dbfs: float       # signal level entering this stage
  gain_db: float                # gain applied at this stage
  output_level_dbfs: float      # signal level leaving this stage
  headroom_db: float            # distance from 0 dBFS (clipping)
  clip_risk: bool               # true if headroom < 3 dB
```

The router tracks gain staging across the entire pipeline and warns when
headroom is insufficient. This feeds into the monitoring system as
`audio.gain_stage.clip_risk` events.

#### Dither

When audio passes through a bit-depth conversion (e.g., 32-bit float
processing → 24-bit output), truncation introduces quantisation distortion.
Dither adds shaped noise to mask this:

```yaml
DitherConfig:
  enabled: bool                 # default: true when bit depth decreases
  type: string                  # "tpdf" (triangular PDF, default), "rpdf" (rectangular),
                                # "hp_tpdf" (high-pass TPDF), "noise_shaped"
  auto: bool                    # automatically apply when format negotiation
                                # selects a lower bit depth at any hop
```

Dither is modelled as a property of format conversion, not a separate device.
When the format negotiation engine (§4.3) fixates a format that reduces bit
depth, dither is applied automatically unless explicitly disabled. The
converter plugin (§6.4) handles this.

#### Professional audio transports

Additional transport types for pro audio installations:

| Transport | Channels | Latency | Use case |
|-----------|----------|---------|----------|
| AES67 | 8–64 | <1ms (LAN) | Studio networked audio (Dante-compatible) |
| Dante | 2–512 | <1ms (LAN) | Broadcast, live sound, installed AV |
| MADI | 64 | <0.5ms | Studio multitrack (BNC/fibre) |
| ADAT | 8 | <0.5ms | Studio interface interconnect (TOSLINK) |
| AES3 (AES/EBU) | 2 | <0.1ms | Studio master clock + digital audio (XLR) |
| S/PDIF | 2 | <0.1ms | Consumer digital audio (coax/TOSLINK) |

These are transport plugins (§6.1). AES67 and Dante are network transports
(UDP/RTP with PTP clock). MADI, ADAT, AES3, and S/PDIF are physical
transports that require specific hardware interfaces — they appear in the
graph as device ports with fixed capabilities.

#### Active redundancy

Professional installations require redundant audio paths — two independent
network paths carrying the same audio, with automatic failover:

```yaml
RedundantPipeline:
  primary: Pipeline             # primary audio path
  secondary: Pipeline           # secondary audio path (different physical route)
  mode: string                  # "active_active" (both running, receiver selects),
                                # "active_standby" (secondary warm, activates on failure)
  switchover_ms: float          # maximum time to switch from primary to secondary
  monitoring: bool              # continuously monitor both paths for differential errors
```

This extends the pipeline model (§2.4). Both pipelines carry the same audio
data. The receiver compares packet sequence numbers and selects from
whichever path is delivering. On failure, switchover is seamless (packets
are already arriving on the secondary path in `active_active` mode).

Dante natively supports primary/secondary redundancy. AES67 achieves it
via IGMP multicast on two independent network paths. The routing graph
models both pipelines and their independent link health.

#### Sample-accurate synchronisation

When multiple audio sources must be time-aligned (multi-machine recording,
distributed DAW), they need a shared sample clock:

```yaml
SampleClockSync:
  mode: string                  # "ptp" (IEEE 1588), "word_clock" (BNC),
                                # "aes_clock" (AES11), "internal" (free-run)
  master: DeviceRef?            # which device is the clock master
  rate: uint                    # sample rate (all devices must agree)
  lock_status: string           # "locked", "locking", "unlocked", "freewheel"
  offset_samples: int?          # measured offset from master (for alignment)
```

This extends the clock model (§7). PTP provides sub-microsecond sync over
Ethernet (sufficient for sample accuracy at 192kHz — one sample = 5.2µs).
Word clock and AES11 are hardware clock distribution standards used in
studios. The routing graph tracks lock status and raises events when sync
is lost.

On PipeWire, the clock master maps to a PipeWire driver node — the device
whose hardware clock drives the graph scheduling. When Ozma manages the
clock, it sets the appropriate PipeWire node as the driver.

#### Spatial audio

The routing graph knows the physical position and orientation of every
speaker (§2.1 `PhysicalLocation` — `pos` + `rot`), the speaker's
directional characteristics (§15 `SpeakerSpatialSpec` — dispersion angles,
directivity), and the listener's position (inferred from zone or explicitly
set). This is sufficient to build spatial audio:

**Speaker arrangement**:

```yaml
SpeakerArrangement:
  id: string
  name: string                  # "Studio Monitors", "Living Room 5.1", "Desk Stereo"
  speakers: SpeakerPlacement[]
  listener: ListenerPosition
  room: RoomAcoustics?          # optional room model for correction
  channel_assignment: ChannelAssignment  # how channels map to speakers

SpeakerPlacement:
  device_id: string             # speaker device in the graph
  location: PhysicalLocation    # position + orientation (§2.1)
  role: string                  # "front_left", "front_right", "centre", "sub",
                                # "surround_left", "surround_right", "rear_left",
                                # "rear_right", "height_front_left", etc.
  distance_m: float?            # measured distance from listener (overrides computed)
  angle_deg: float?             # measured angle from listener (overrides computed)
  elevation_deg: float?         # measured elevation from listener (overrides computed)
  level_trim_db: float?         # per-speaker level trim

ListenerPosition:
  pos: Position3d               # listener's head position in world space (mm)
  facing: float                 # yaw angle the listener faces (degrees, 0 = +X)
  ear_height_mm: float?         # ear height above floor (default: seated ~1100mm)
  source: string                # "manual", "furniture_derived", "zone_centre"
  # If source is "furniture_derived": computed from the chair/desk position
  # If source is "zone_centre": computed from the zone bounds centre point

ChannelAssignment:
  mode: string                  # "itu" (standard angles), "measured" (actual angles),
                                # "phantom" (virtual speakers from fewer physical speakers)
  assignments: ChannelSpeakerMap[]

ChannelSpeakerMap:
  channel: string               # "FL", "FR", "FC", "LFE", "SL", "SR", etc.
  speaker_id: string            # which physical speaker
  gain_db: float?               # trim for this channel→speaker mapping
  delay_ms: float?              # per-channel delay (for distance compensation)
```

**What the location data provides for spatial audio**:

1. **Distance compensation**: Each speaker's distance from the listener is
   known from `pos` coordinates. The closer speaker needs delay added so
   sound from all speakers arrives simultaneously. Computed automatically:
   ```
   delay_ms = (max_distance - this_distance) / speed_of_sound_mm_per_ms
   ```
   Speed of sound ≈ 343,000 mm/s = 343 mm/ms.

2. **Level compensation**: Inverse square law — a speaker 2× farther away
   is 6 dB quieter. The system can auto-trim levels based on distance.

3. **Angle verification**: ITU-R BS.775 recommends specific speaker angles
   (±30° for stereo, ±110° for surrounds). The system knows actual speaker
   angles from position data and can warn when placement deviates from
   standards, or adapt processing to compensate.

4. **Dispersion coverage**: The speaker's dispersion spec (horizontal/vertical
   degrees from device database) combined with its orientation (`rot`) and
   the listener position tells you whether the listener is within the
   speaker's coverage angle. If not, the system warns ("your left surround
   is aimed at the wall, not the listener").

5. **Subwoofer integration**: Subwoofer position relative to walls and
   corners affects bass response. The system knows room dimensions (§2.11
   `Space.dimensions_mm`) and sub position, enabling room mode estimation
   and optimal crossover selection.

6. **Headphone virtualisation**: When the output switches from speakers to
   headphones (e.g., monitor controller speaker set change), the speaker
   arrangement data enables binaural rendering — virtualising the physical
   speaker positions in the headphone soundstage using HRTF.

**Room acoustics** (optional, feeds into room correction):

```yaml
RoomAcoustics:
  dimensions_mm: Dimensions     # room width, depth, height
  rt60_ms: float?               # measured or estimated reverberation time
  treatment: string[]?          # ["absorption_panels", "diffusers", "bass_traps"]
  floor: string?                # "carpet", "hardwood", "concrete", "tile"
  walls: string?                # "drywall", "concrete", "glass", "curtains"
  ceiling: string?              # "drywall", "acoustic_tile", "exposed"
  noise_floor_dba: float?       # measured ambient noise level
```

Room acoustics combined with speaker positions feeds the room correction
system (§2.9 `audio_processor` with `processor_type: "room_correction"`).
The sweep → FFT → parametric EQ pipeline uses speaker position and room
dimensions to optimise the correction curve per speaker.

---

## 3. Intents

An intent declares what the user (or system) wants to achieve with a pipeline.
Intents drive every routing decision — they are not metadata, they are the
primary input to the route calculator.

### 3.1 Intent structure

```yaml
Intent:
  name: string                  # identifier (built-in or user-defined)
  description: string           # human-readable purpose
  streams: StreamIntent[]       # per-media-type requirements
  priority: uint                # relative priority when resources are contended
  degradation: DegradationPolicy # what to do when constraints can't be met
```

**StreamIntent**:

```yaml
StreamIntent:
  media_type: video | audio | hid | screen | rgb | control | data
  required: bool                # must this stream exist for the intent to be satisfied?
  constraints: Constraints      # hard limits — pipeline is rejected if these can't be met
  preferences: Preferences      # soft targets — optimise toward these, accept less
```

**Constraints** (hard — reject pipeline if violated):

```yaml
Constraints:
  max_latency_ms: float?        # end-to-end latency ceiling (steady-state)
  max_activation_time_ms: float? # maximum time to go from current state to active (see §2.6)
  min_bandwidth_bps: uint64?    # minimum available bandwidth required
  max_loss: float?              # maximum acceptable loss rate
  max_jitter_ms: float?         # maximum acceptable jitter
  max_hops: uint?               # maximum number of links traversed
  max_conversions: uint?        # maximum format changes allowed
  required_formats: Format[]?   # must support at least one of these formats
  forbidden_formats: Format[]?  # never use these formats
  encryption: required | preferred | none  # data-in-transit encryption requirement
```

**Preferences** (soft — used for ranking candidate pipelines):

```yaml
Preferences:
  target_latency_ms: float?     # ideal latency
  target_resolution: Resolution? # ideal video resolution
  target_framerate: float?      # ideal framerate
  target_sample_rate: uint?     # ideal audio sample rate
  target_channels: uint?        # ideal audio channel count
  target_bit_depth: uint?       # ideal audio bit depth
  prefer_lossless: bool?        # prefer lossless codecs when available
  prefer_hardware_codec: bool?  # prefer hardware encode/decode
  prefer_fewer_hops: bool?      # weight hop count more heavily
  prefer_lower_latency: bool?   # weight latency more heavily
  prefer_higher_quality: bool?  # weight quality more heavily
```

### 3.2 Built-in intents

These are the standard intents shipped with every Ozma installation. Users can
modify their parameters or define entirely new intents.

#### `control`

Headless operation. Keyboard and mouse only, no video or audio needed.

```yaml
name: control
description: Keyboard and mouse input to a headless or blind target
priority: 90
streams:
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
      max_loss: 0.001
    preferences:
      prefer_lower_latency: true
  - media_type: video
    required: false
  - media_type: audio
    required: false
```

#### `preview`

Low-resolution, high-latency video for thumbnails and machine pickers. Cheap to
produce and transport.

```yaml
name: preview
description: Low-res thumbnail for dashboards, machine picker, monitoring grid
priority: 10
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 2000
    preferences:
      target_resolution: { w: 640, h: 360 }
      target_framerate: 2
      prefer_lower_latency: false
      prefer_higher_quality: false
  - media_type: hid
    required: false
  - media_type: audio
    required: false
```

#### `observe`

High-resolution monitoring where latency is secondary. Security cameras, kiosk
checks, passive monitoring.

```yaml
name: observe
description: High-quality passive monitoring — latency is acceptable
priority: 20
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 1000
    preferences:
      target_resolution: { w: 1920, h: 1080 }
      target_framerate: 15
      prefer_higher_quality: true
  - media_type: audio
    required: false
    constraints:
      max_latency_ms: 1000
    preferences:
      target_channels: 1
      prefer_lossless: false
  - media_type: hid
    required: false
```

#### `desktop`

General productivity. Good video, reasonable latency, stereo audio. The default
for most KVM usage.

```yaml
name: desktop
description: General productivity — good video, reasonable latency, stereo audio
priority: 50
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 100
    preferences:
      target_resolution: { w: 1920, h: 1080 }
      target_framerate: 30
      prefer_lower_latency: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 50
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      target_bit_depth: 16
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 10
      max_loss: 0.0001
```

#### `creative`

Video/audio production, CAD, music. High resolution, low latency, multichannel
lossless audio.

```yaml
name: creative
description: Content creation — high resolution, low latency, lossless audio
priority: 70
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 50
    preferences:
      target_resolution: { w: 3840, h: 2160 }
      target_framerate: 60
      prefer_higher_quality: true
      prefer_hardware_codec: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 20
      forbidden_formats:
        - { codec: opus }
        - { codec: aac }
        - { codec: mp3 }
    preferences:
      target_channels: 8
      target_sample_rate: 96000
      target_bit_depth: 24
      prefer_lossless: true
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
```

#### `gaming`

Interactive gaming. Native resolution, minimum latency for both video and input.
Gamepad support.

```yaml
name: gaming
description: Interactive gaming — minimum latency, native resolution, gamepad
priority: 80
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 16
    preferences:
      target_framerate: 60
      prefer_lower_latency: true
      prefer_hardware_codec: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 20
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      prefer_lower_latency: true
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
      max_loss: 0.0001
    preferences:
      prefer_lower_latency: true
degradation:
  video: reduce_framerate_first
  audio: allow_lossy_compression
  hid: never_degrade
```

#### `broadcast`

One-to-many distribution. Source quality, latency is irrelevant. Teacher mode,
screen share, recording source.

```yaml
name: broadcast
description: One-to-many distribution — source quality, latency irrelevant
priority: 30
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 5000
      max_conversions: 1
    preferences:
      prefer_higher_quality: true
      prefer_fewer_hops: true
  - media_type: audio
    required: false
    constraints:
      max_latency_ms: 5000
    preferences:
      target_channels: 2
      prefer_higher_quality: true
  - media_type: hid
    required: false
```

#### `fidelity_audio`

Audio is the primary concern. No compression, ever. Video and HID are
irrelevant. Music listening, mixing, mastering, room correction measurement.

```yaml
name: fidelity_audio
description: Uncompromised audio — never compressed, multichannel, high sample rate
priority: 60
streams:
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 10
      forbidden_formats:
        - { codec: opus }
        - { codec: aac }
        - { codec: mp3 }
        - { codec: vorbis }
        - { codec: wma }
        - { lossy: true }
    preferences:
      target_channels: 8
      target_sample_rate: 96000
      target_bit_depth: 32
      prefer_lossless: true
  - media_type: video
    required: false
  - media_type: hid
    required: false
```

### 3.3 Intent composition

Intents are composable. A user may want `control` + `fidelity_audio` (typing on
a headless audio workstation). When intents are composed:

1. Each intent's stream requirements are merged per media type
2. Constraints are intersected (the strictest constraint wins)
3. Preferences are merged (conflicts resolved by higher-priority intent)
4. Priority is the maximum of the composed intents

Composition produces a new synthetic intent that the router treats identically
to a named intent.

### 3.4 Degradation policy

When the graph cannot satisfy an intent's constraints, the degradation policy
determines what happens:

```yaml
DegradationPolicy:
  # per-media-type degradation strategy
  video: DegradationStrategy
  audio: DegradationStrategy
  hid: DegradationStrategy

DegradationStrategy: enum
  never_degrade       # fail the pipeline rather than degrade this stream
  reduce_framerate_first   # video: drop framerate before resolution
  reduce_resolution_first  # video: drop resolution before framerate
  allow_lossy_compression  # audio: permit lossy codec as last resort
  reduce_sample_rate       # audio: lower sample rate before allowing lossy
  increase_latency         # accept higher latency to maintain quality
  drop_stream              # remove this stream entirely rather than degrade others
```

The router applies degradation strategies in order of the intent's stream
priorities. HID is almost never degraded — it's tiny bandwidth and critical
for usability. Audio next. Video last (it has the most room to degrade
gracefully).

### 3.5 Custom intents

Users define custom intents in the same YAML schema. Custom intents can extend
built-in ones:

```yaml
name: my_streaming_setup
extends: broadcast
description: My Twitch streaming with specific audio requirements
streams:
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 100
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      target_bit_depth: 24
      prefer_lossless: true
```

Fields in the child override the parent. Streams are merged by `media_type` —
a child stream definition replaces the parent's definition for that media type.

---

## 4. Format System

Formats describe the shape of data at any point in the graph. Format negotiation
determines what data looks like on each link in a pipeline.

### 4.1 Format structure

A format is a media-type-specific description of data shape:

```yaml
Format:
  media_type: video | audio | hid | screen | rgb | control | data
  # one of the following, depending on media_type:
  video: VideoFormat?
  audio: AudioFormat?
  hid: HidFormat?
  screen: ScreenFormat?         # rendered frames/widgets for small screens (see §2.8)
  rgb: RgbFormat?               # LED color data for RGB endpoints (see §2.8)
  control: ControlFormat?       # control surface input/feedback (see §2.8)
  data: DataFormat?
```

**VideoFormat**:

```yaml
VideoFormat:
  codec: string                 # "raw", "h264", "h265", "av1", "vp9", "mjpeg", "ndi"
  container: string?            # "rtp", "rtsp", "hls", "mpegts", "raw", null
  resolution: Resolution        # { w: uint, h: uint }
  framerate: float              # frames per second
  color_space: string?          # "bt709", "bt2020", "srgb"
  bit_depth: uint?              # 8, 10, 12
  hdr: bool?                    # HDR metadata present
  chroma_subsampling: string?   # "4:4:4", "4:2:2", "4:2:0"
  bitrate_bps: uint64?          # for compressed codecs
  profile: string?              # codec profile ("main", "high", "baseline")
  level: string?                # codec level
  keyframe_interval: uint?      # frames between keyframes
  lossy: bool                   # true for all compressed codecs except lossless modes
```

**AudioFormat**:

```yaml
AudioFormat:
  codec: string                 # "pcm", "opus", "aac", "flac", "vban", "aes67"
  container: string?            # "rtp", "vban", "raw", null
  sample_rate: uint             # Hz (44100, 48000, 96000, 192000)
  channels: uint                # 1=mono, 2=stereo, 6=5.1, 8=7.1
  bit_depth: uint               # 16, 24, 32
  sample_format: string?        # "int", "float"
  bitrate_bps: uint64?          # for compressed codecs
  frame_size: uint?             # samples per frame/packet
  lossy: bool                   # true for opus, aac, mp3, etc.
  channel_layout: string?       # "stereo", "5.1", "7.1", "7.1.4", "custom" (see §2.13)
  channel_map: string[]?        # per-channel labels: ["FL","FR","FC","LFE","SL","SR"]
```

**HidFormat**:

```yaml
HidFormat:
  device_type: string           # "keyboard", "mouse", "gamepad", "tablet", "consumer"
  report_rate_hz: uint          # how often reports are sent
  report_size_bytes: uint       # size of each report
  protocol: string              # "boot", "report", "ozma-extended"
  absolute_positioning: bool    # true for tablets/touchscreens, mouse uses absolute
```

**ScreenFormat** (rendered content for small/embedded screens — see §2.8):

```yaml
ScreenFormat:
  encoding: string              # "raw_rgb", "raw_rgb565", "jpeg", "png",
                                # "widget_def", "typed_data"
  resolution: Resolution?       # { w: uint, h: uint } — native panel resolution
                                # (null for typed_data — device handles layout)
  framerate: float?             # target refresh rate (null for event-driven updates)
  color_depth: uint?            # bits per pixel (16 for RGB565, 24 for RGB888)
  color_space: string?          # "srgb", "monochrome"
  rotation: uint?               # 0, 90, 180, 270 — panel orientation
  dithering: bool?              # whether the sink supports dithering (for low-depth panels)
  partial_update: bool?         # can the sink accept partial frame updates?
  rendering_tier: uint          # 0=push raw frames, 1=server-rendered,
                                # 2=native render from widget defs,
                                # 3=data-driven (device has its own UI, consumes typed data)
  data_schema: DataSchema?      # for tier 3: what data fields the device accepts
```

Screen encoding `typed_data` (tier 3) is for devices that have their own
display logic and UI — they don't receive frames or widget definitions, they
receive structured data and render it themselves. Examples: an ESP32 with a
custom firmware that shows temperature/status, a phone app showing scenario
state, a wall-mounted tablet running its own dashboard, or an e-ink display
that formats its own layout from received values.

**DataSchema** — describes what typed data a tier-3 screen accepts:

```yaml
DataSchema:
  fields: DataField[]           # what the device wants to receive
  update_mode: string           # "event" (on change), "poll" (periodic), "push" (controller decides)
  max_update_rate_hz: float?    # maximum rate the device can handle (null = unlimited)

DataField:
  key: string                   # field identifier (e.g., "cpu_temp", "active_scenario", "node_status")
  type: string                  # "string", "number", "bool", "enum", "timestamp", "list", "object"
  unit: string?                 # for numbers: "celsius", "percent", "bytes", "bps", etc.
  enum_values: string[]?        # for enum type: allowed values
  description: string?          # human-readable description
  required: bool                # must the controller provide this field?
  default: any?                 # value to display if not provided
```

This makes `data` a first-class format for screen endpoints. The controller
publishes data, the device subscribes to what it needs. A tier-3 device might
advertise that it accepts `{active_scenario: string, node_count: number,
cpu_temp: number, alerts: list}` and the controller pushes updates when those
values change. The device renders however it wants — the controller doesn't
know or care about the device's UI.

**RgbFormat** (LED color data for addressable LEDs, RGB zones, fixtures — see §2.8):

```yaml
RgbFormat:
  encoding: string              # "rgb888", "rgb565", "rgbw", "hsv", "ddp", "artnet", "e131"
  led_count: uint               # number of individually addressable LEDs
  framerate: float              # refresh rate (typically 30–60 fps)
  zones: uint?                  # number of addressable zones (if not per-LED)
  color_depth: uint             # bits per channel (8 for RGB888, 5/6/5 for RGB565)
  white_channel: bool?          # RGBW support
  gamma_corrected: bool?        # whether data is pre-gamma-corrected
```

**ControlFormat** (control surface input and feedback — see §2.8):

```yaml
ControlFormat:
  protocol: string              # "midi", "osc", "hid_gamepad", "hid_consumer",
                                # "streamdeck", "shuttlepro", "evdev", "serial"
  inputs: ControlInputSet?      # what the device can send (buttons, faders, encoders)
  outputs: ControlOutputSet?    # what the device can receive (LEDs, displays, motor faders)
  report_rate_hz: uint?         # input report rate
  bidirectional: bool           # does this device accept feedback?
```

**ControlInputSet**:

```yaml
ControlInputSet:
  buttons: uint?                # number of buttons/keys
  faders: uint?                 # number of faders/sliders
  encoders: uint?               # number of rotary encoders
  xy_pads: uint?                # number of XY touch pads
  axes: uint?                   # number of analog axes (gamepad sticks, triggers)
  pressure_sensitive: bool?     # velocity/pressure on buttons
  touch_strips: uint?           # number of touch strips
```

**ControlOutputSet** (feedback capabilities):

```yaml
ControlOutputSet:
  button_leds: uint?            # number of LED-backlit buttons
  led_colors: uint?             # color depth per LED (1=on/off, 3=RGB, etc.)
  displays: ScreenEndpoint[]?   # embedded screens (e.g., Stream Deck LCD, X-Touch scribble strip)
  motor_faders: uint?           # number of motorised faders
  led_rings: uint?              # number of encoder LED rings
  rumble_motors: uint?          # haptic feedback motors
```

**DataFormat** (for arbitrary data streams — clipboard, file transfer, etc.):

```yaml
DataFormat:
  encoding: string              # "raw", "protobuf", "json", "msgpack"
  schema: string?               # schema identifier if applicable
  max_message_size: uint?       # maximum single message size in bytes
```

### 4.2 FormatSet — capability advertisement

A FormatSet describes everything a port can handle. It's a list of formats with
optional ranges:

```yaml
FormatSet:
  formats: FormatRange[]

FormatRange:
  media_type: video | audio | hid | screen | rgb | control | data
  # Ranges allow compact expression of capabilities.
  # Each field can be a single value, a list of accepted values, or a range.
  video:
    codec: ["h264", "h265", "mjpeg"]           # any of these
    resolution: { min: {w: 320, h: 240}, max: {w: 3840, h: 2160} }  # range
    framerate: { min: 1, max: 60 }             # range
    # ... other fields follow the same pattern
```

This is directly inspired by PipeWire's SPA format enumeration — ports advertise
ranges, and the router computes intersections to find compatible formats.

### 4.3 Format negotiation

Format negotiation follows PipeWire's three-phase model, adapted for distributed
operation:

**Phase 1: Enumerate**

Each port in a candidate pipeline reports its FormatSet. The router collects all
of them.

**Phase 2: Restrict**

The router computes the intersection of FormatSets across each link. If the
intersection is empty, a converter must be inserted (adding a hop) or the
pipeline is rejected.

The intent's constraints and preferences further restrict the intersection:
- Constraints remove formats that violate hard limits
- Preferences rank remaining formats

**Phase 3: Fixate**

The router selects one concrete format per link. Selection criteria (in order):

1. Satisfies all constraints
2. Minimises conversions (prefer native format passthrough)
3. Matches preferences (resolution, framerate, codec, etc.)
4. Prefers hardware-accelerated codecs if `prefer_hardware_codec` is set
5. Among equal candidates, prefer lower bandwidth consumption

Fixation is **deterministic** — given the same graph, intent, and measurements,
the router always selects the same format. This makes pipelines predictable and
debuggable.

**Pre-computation**: Because the graph topology is known ahead of time (devices
are discovered, capabilities are enumerated, links are measured), format
negotiation is not a runtime handshake. The router pre-computes pipelines and
their formats. When a pipeline is activated, the format is already decided. This
is fundamentally different from RTSP/SDP negotiation, which happens at session
setup time. Ozma's negotiation happens at graph-change time — device discovery,
link metric updates, intent changes.

### 4.4 Bandwidth calculation

Every format implies a bandwidth requirement. The router calculates this:

**Uncompressed video**: `width × height × bit_depth × channels × framerate` bits/sec
- Example: 1920×1080×24×1×30 = 1,492,992,000 bps (~1.5 Gbps)

**Compressed video**: `bitrate_bps` (from codec profile/level or measured)
- Example: H.264 1080p30 high profile ≈ 8,000,000 bps (8 Mbps)

**Uncompressed audio**: `sample_rate × channels × bit_depth` bits/sec
- Example: 48000×2×16 = 1,536,000 bps (1.5 Mbps)
- Example: 96000×8×24 = 18,432,000 bps (18.4 Mbps)

**Compressed audio**: `bitrate_bps`
- Example: Opus stereo ≈ 128,000 bps (128 Kbps)

**HID**: `report_rate_hz × report_size_bytes × 8` bits/sec
- Example: keyboard at 1000 Hz × 8 bytes = 64,000 bps (64 Kbps)
- Example: mouse at 1000 Hz × 6 bytes = 48,000 bps (48 Kbps)

HID bandwidth is negligible compared to audio and video. This is why HID is
almost never the bottleneck and should never be degraded.

**Screen (raw frames)**: `width × height × color_depth × framerate` bits/sec
- Example: Stream Deck XL (96×96 per key × 32 keys as one 480×384 frame) × 24 × 15 fps
  = 66,355,200 bps (~66 Mbps raw, ~2 Mbps JPEG)
- Example: Corsair iCUE 480×480 × 24 × 30 fps = 165,888,000 bps (~166 Mbps raw, ~5 Mbps JPEG)
- Example: e-ink 400×300 × 1bpp × 0.2 fps = 24,000 bps (negligible)

**Screen (widget definitions)**: Negligible — JSON payloads, typically <1 KB per update,
updates only on state change. Native-rendering devices (tier 2) receive definitions,
not frames.

**RGB**: `led_count × color_depth × framerate` bits/sec
- Example: 300-LED WLED strip × 24 × 30 fps = 216,000 bps (~216 Kbps)
- Example: 104-key RGB keyboard × 24 × 30 fps = 74,880 bps (~75 Kbps)
- Example: Full room (1500 LEDs) × 24 × 60 fps = 2,160,000 bps (~2.2 Mbps)
- With protocol overhead (DDP/E1.31/ArtNet): typically 1.5–2× raw

**Control surface input**: `report_rate × report_size × 8` bits/sec
- Example: MIDI at 31.25 kbaud = 31,250 bps (protocol maximum)
- Example: Stream Deck buttons at 100 Hz × 2 bytes = 1,600 bps
- Example: Gamepad at 250 Hz × 12 bytes = 24,000 bps

**Control surface feedback**: Varies widely by device
- Example: Stream Deck key images (15 keys × JPEG ≈ 3 KB each) on state change
  = bursty, ~45 KB per update, rare
- Example: X-Touch scribble strips (8 × 7 chars × 2 lines) = <100 bytes per update
- Example: Motor fader position (8 faders × 2 bytes × 50 Hz) = 6,400 bps

Screen, RGB, and control surface bandwidth is small compared to video and
audio. However, the rendering cost on the controller can be significant — see
§2.7 for device capacity and §2.8 for endpoint-specific resource accounting.

---

## 5. Information Quality

Every measured or reported property in the graph carries a provenance tag
indicating how much the router should trust it.

### 5.1 Quality levels

```yaml
InfoQuality: enum
  user        # explicitly set by the user — highest trust
  measured    # from active probing or passive measurement — high trust
  inferred    # derived from measured data + known/assumed parameters (see §2.10) — high-medium trust
  reported    # from OS/driver API (lsusb, boltctl, WMI, PipeWire) — medium trust
  commanded   # we sent a command but cannot confirm it was applied — medium-low trust
  spec        # from device specification or standard (USB 3.0 = 5 Gbps) — low trust
  assumed     # heuristic or default — lowest trust, override as soon as possible
```

**Trust ordering**: `user > measured > inferred > reported > commanded > spec > assumed`

When multiple quality levels exist for the same property, the highest-trust
value is used. When `measured` contradicts `spec` (USB 3.0 port only achieving
USB 2.0 speeds behind a hub), `measured` wins.

**`commanded` quality**: This level exists specifically for write-only devices
(see §2.5). When the router sends a command to a device (e.g., "switch to
input 3") but receives no confirmation, the resulting state is `commanded`. The
router treats `commanded` as better than a blind assumption (`assumed`) because
we actively did something, but worse than `reported` because we can't verify it
worked. If a pipeline built on `commanded` state fails to deliver data, the
router knows the switch state is the likely culprit and can retry, try an
alternative path, or escalate to the user.

`commanded` values do not decay in the same way as `measured` values (§5.3) —
they remain `commanded` indefinitely until either confirmed by measurement
(e.g., video starts flowing, upgrading to `measured`) or contradicted by
observation (e.g., no video after timeout, downgrading to `assumed`).

### 5.2 Quality metadata

Every property that carries a quality tag also carries metadata about the
measurement:

```yaml
QualifiedValue<T>:
  value: T                      # the actual value
  quality: InfoQuality          # provenance
  source: string                # where this came from ("lsusb", "iperf", "user override")
  measured_at: timestamp?       # when this was measured (null for spec/assumed)
  confidence: float?            # 0.0–1.0, statistical confidence (for measured values)
  sample_count: uint?           # number of measurements (for measured values)
```

### 5.3 Quality decay

Measured values decay over time. A bandwidth measurement from 5 minutes ago is
less trustworthy than one from 5 seconds ago. The router applies a decay
function:

```
effective_quality = measured    if age < fresh_threshold
effective_quality = reported   if age > stale_threshold
```

Thresholds are configurable. Defaults:
- `fresh_threshold`: 30 seconds
- `stale_threshold`: 5 minutes

Between thresholds, the router uses the measured value but flags it as
potentially stale. Re-measurement is triggered proactively for stale values on
active pipelines.

### 5.4 Quality in routing decisions

The router uses quality levels in two ways:

1. **Confidence weighting**: When comparing two candidate pipelines, the one with
   higher-quality measurements is preferred (all else being equal). A pipeline
   built on `measured` data is more trustworthy than one built on `assumed` data.

2. **Uncertainty budgeting**: Properties with `assumed` or `spec` quality get a
   safety margin applied. If a USB 3.0 port's bandwidth is `spec` quality
   (5 Gbps), the router treats it as 4 Gbps for capacity planning. If it's
   `measured` at 4.8 Gbps, the router uses 4.8 Gbps.

---

## 6. Plugin Contracts

Everything that is not a core graph primitive is a plugin. Transports, devices,
codecs, converters, and switches are all plugin types with defined interfaces.

### 6.0 Plugin registration and lifecycle

Plugins are the extension mechanism for the entire routing system. The built-in
transports, device plugins, and codecs that ship with Ozma are themselves
plugins — they use the same registration and lifecycle as third-party plugins.
There is no distinction between "core" and "external" at the interface level.

**Plugin registration**:

```yaml
PluginManifest:
  id: string                    # globally unique plugin ID ("com.example.lora-transport")
  name: string                  # human-readable name ("LoRa Transport Plugin")
  version: string               # semver
  type: PluginType              # what kind of plugin this is
  description: string?
  author: string?
  license: string?
  url: string?                  # project/documentation URL
  min_ozma_version: string?     # minimum Ozma version required
  max_ozma_version: string?     # maximum Ozma version tested with
  dependencies: string[]?       # other plugins this depends on
  platforms: string[]?          # supported platforms (null = all)
  python_package: string?       # PyPI package name (for pip install)
  entry_point: string           # Python module:class path

PluginType: enum
  transport                     # moves data between ports (§6.1)
  device                        # discovers hardware/software devices (§6.2)
  codec                         # encodes/decodes media (§6.3)
  converter                     # transforms data between formats (§6.4)
  switch                        # controls external switching devices (§6.5)
  composite                     # provides multiple of the above (e.g., a plugin
                                # that discovers LoRa devices AND provides LoRa transport)
```

**Plugin lifecycle**:

```
discover → load manifest → check compatibility → instantiate → register → start
  │                                                                         │
  │  On shutdown or unload:                                                │
  └── stop → deregister → unload                                          │
                                                                           │
  Running:                                                                 │
  ├── graph queries (discover_links, capabilities, etc.)                   │
  ├── active operations (open, close, measure)                            │
  └── events (on_link_change, on_hotplug, on_state_change)               │
```

**Registration methods**:

1. **Built-in**: Ship with Ozma. Loaded automatically on startup. Cannot be
   unloaded. These are the default transports, device plugins, and codecs.

2. **Installed**: Installed via `pip install` or placed in the plugins
   directory. Loaded on startup if present. Can be disabled via config.
   ```
   pip install ozma-plugin-lora
   # or
   cp lora_transport.py ~/.config/ozma/plugins/
   ```

3. **Dynamic**: Loaded at runtime via the API. Useful for development and
   testing. Can be loaded and unloaded without restart.
   ```
   POST /api/v1/plugins/load    { "entry_point": "my_plugin:MyTransport" }
   DELETE /api/v1/plugins/{id}   # unload
   ```

**Plugin isolation**: Plugins run in the controller's process but are
sandboxed by convention — they must only interact with the system through
the plugin interface methods. A misbehaving plugin (crash, hang, excessive
resource use) is caught by the plugin host, logged, and disabled. The
controller continues operating without it.

**Language stability guarantee**: The plugin interface is defined in Python.
This is a stable contract — plugins written in Python today will continue
to work if the controller core is rewritten in another language (e.g., Rust
via PyO3 embedded interpreter). The Python plugin interface is the public
API; the controller's implementation language is an internal detail. Plugin
authors should not depend on the controller being Python — only on the
plugin interface classes and the graph query API they expose.

**Plugin API**:

```
GET /api/v1/plugins                     # list all registered plugins
GET /api/v1/plugins/{id}                # plugin detail (manifest, status, metrics)
POST /api/v1/plugins/load               # load a dynamic plugin
DELETE /api/v1/plugins/{id}             # unload a dynamic plugin
PUT /api/v1/plugins/{id}/config         # update plugin configuration
GET /api/v1/plugins/{id}/config         # get plugin configuration
POST /api/v1/plugins/{id}/enable        # enable a disabled plugin
POST /api/v1/plugins/{id}/disable       # disable without unloading
```

**What a plugin can do**:

- Add new device types to the graph (via device plugin interface)
- Add new transport types (via transport plugin interface)
- Add new codec/converter types (via codec/converter plugin interface)
- Add new switch controller types (via switch plugin interface)
- Define new `TransportCharacteristics` baselines for its transport
- Contribute to the device database (add entries for devices it discovers)
- Emit events on the WebSocket stream (namespaced: `plugin.{id}.{event}`)
- Register API endpoints (namespaced: `/api/v1/plugins/{id}/{path}`)
- Read the routing graph (query devices, ports, links, pipelines)
- Receive graph change notifications (device added/removed, link state change)

**What a plugin cannot do**:

- Modify other plugins' state
- Bypass the transport encryption model (§10)
- Access the filesystem outside its data directory
- Register core API endpoints (only namespaced under `/api/v1/plugins/{id}/`)
- Override built-in plugin behaviour (but can provide alternatives)

**Example — a LoRa transport plugin**:

```python
# ozma_plugin_lora/transport.py
from ozma.plugins import TransportPlugin, PluginManifest

class LoRaTransport(TransportPlugin):
    manifest = PluginManifest(
        id="community.lora-transport",
        name="LoRa Transport",
        version="0.1.0",
        type="transport",
        entry_point="ozma_plugin_lora.transport:LoRaTransport",
    )

    # TransportPlugin interface methods:
    async def discover_links(self): ...
    async def capabilities(self, link): ...
    async def measure(self, link): ...
    async def open(self, link, format): ...
    async def close(self, stream): ...
    def on_link_change(self, callback): ...
```

Once loaded, the LoRa transport appears in the graph like any other transport.
Links discovered by the LoRa plugin have the LoRa transport type, LoRa-specific
characteristics, and participate in routing decisions. The router doesn't know
or care that it came from a plugin — it's just another transport with a cost.

### 6.1 Transport plugin

A transport moves data between two ports, potentially across machine boundaries.

```yaml
TransportPlugin:
  id: string                    # "udp-direct", "wireguard", "hdmi-loopback",
                                # "usb-gadget", "pipewire", "local-pipe"
  name: string                  # human-readable name
  media_types: MediaType[]      # which media types this transport can carry
  requires_network: bool        # does this transport need IP connectivity?
  supports_multicast: bool      # can one sender reach multiple receivers?
  supports_encryption: bool     # can this transport encrypt data in transit?
  encryption_overhead_bps: uint? # bandwidth overhead of encryption
  expected_characteristics: TransportCharacteristics  # baseline assumptions before measurement

  # --- Methods ---

  discover_links():
    # Returns all links that exist via this transport.
    # Called during topology discovery.
    returns: Link[]

  capabilities(link: LinkRef):
    # Returns the FormatSet this link can carry.
    returns: FormatSet

  measure(link: LinkRef):
    # Actively probes the link and returns measured properties.
    # This may inject probe traffic (bandwidth test, latency ping).
    returns: LinkMetrics { bandwidth, latency, jitter, loss }

  open(link: LinkRef, format: Format):
    # Activates the link with the specified format.
    # Returns a stream handle for data flow.
    returns: StreamHandle

  close(stream: StreamHandle):
    # Tears down the stream.

  on_link_change(callback):
    # Notifies when links appear, disappear, or change state.
    # (e.g., USB hotplug, network interface up/down)
```

#### Multiplexed connections

Inspired by SSH's channel model, a single transport connection between two
devices can carry multiple typed channels simultaneously. Rather than
opening separate connections for HID, audio, video, and control (4
handshakes, 4 keepalive loops, 4 failure modes), a multiplexed connection
provides one authenticated, encrypted tunnel with independent channels
inside.

```yaml
MultiplexedConnection:
  id: string
  transport: string             # underlying transport ("udp-aead", "wireguard", etc.)
  local: DeviceRef              # this end
  remote: DeviceRef             # other end
  state: string                 # "establishing", "active", "rekeying", "draining", "closed"
  channels: Channel[]           # active channels on this connection
  session: SessionState         # encryption session (keys, counters, rekey schedule)
  keepalive: KeepaliveConfig    # connection health monitoring
  shared_by: PipelineRef[]      # which pipelines share this connection

Channel:
  id: uint                      # channel number (unique within connection)
  type: string                  # media type this channel carries
  name: string                  # human-readable ("hid", "audio-vban", "video-h264", "control")
  priority: ChannelPriority     # scheduling priority
  flow_control: FlowControl     # per-channel flow management
  state: string                 # "open", "half_closed", "closed"
  format: Format?               # negotiated format on this channel
  stats: ChannelStats           # per-channel metrics

ChannelPriority: enum
  realtime                      # HID, control commands — never delayed, tiny packets
  high                          # audio — low latency, moderate bandwidth
  normal                        # video — high bandwidth, can tolerate brief delays
  low                           # sensors, RGB, screen updates — best effort
  bulk                          # file transfer, firmware upload — use remaining bandwidth

FlowControl:
  window_bytes: uint?           # per-channel receive window (backpressure)
  max_packet_size: uint?        # maximum payload per packet on this channel
  rate_limit_bps: uint64?       # optional rate cap on this channel
```

**Priority scheduling**: When the underlying transport has limited bandwidth
(WiFi, WireGuard over internet, serial), the connection multiplexer ensures
`realtime` channels (HID) are never starved by `normal` channels (video).
A keypress is 8 bytes and must go out immediately; a video frame is 50 KB
and can wait one packet slot. This is strict priority: higher priority
channels are always serviced first. Within the same priority, round-robin.

On high-bandwidth transports (wired Gigabit LAN), priority scheduling is
irrelevant — there's enough bandwidth for everything simultaneously. The
multiplexer detects this and skips the scheduling overhead.

**Session rekeying**: Long-lived connections (a node that's been connected
for weeks) rotate encryption keys periodically without dropping the
connection. Default: rekey every 1 GB of data or every 1 hour, whichever
comes first. During rekeying, data continues to flow — the old keys are
used until the new keys are established, then traffic switches atomically.

```yaml
RekeyPolicy:
  max_bytes: uint64             # rekey after this many bytes (default: 1 GB)
  max_seconds: uint             # rekey after this many seconds (default: 3600)
  algorithm: string             # key exchange algorithm ("noise_xx", "noise_nk")
```

**Connection sharing**: Multiple pipelines between the same pair of devices
share a single multiplexed connection. Opening a video pipeline to a node
that already has an HID pipeline reuses the existing connection and adds a
video channel. Closing the video pipeline removes the channel but keeps the
connection alive for HID. The connection is torn down only when the last
channel closes (or on keepalive failure).

**Subsystem advertisement**: When a connection is established, each side
advertises which channel types it supports:

```yaml
SubsystemAdvertisement:
  supported: SubsystemCapability[]

SubsystemCapability:
  name: string                  # "hid", "audio", "video", "control", "sensors",
                                # "rgb", "screen", "serial_console", "file_transfer"
  formats: FormatSet            # what formats this subsystem accepts/produces
  max_channels: uint?           # maximum simultaneous channels of this type
```

The controller opens only the subsystems it needs. A node with no capture
card doesn't advertise `video`. A display-only node advertises `screen` but
not `hid`. This is capability negotiation at the connection level — before
any pipeline is assembled.

**Relationship to the transport plugin interface**: Multiplexing is an
optional capability of a transport plugin. Transports that support it
expose channels; transports that don't (e.g., a raw serial link, a VBAN
UDP stream) carry a single data type per link as before. The `open()`
method on the transport plugin returns either a simple `StreamHandle` or
a `MultiplexedConnection` with channel management methods. The router
adapts to whichever the transport provides.

**TransportCharacteristics** — baseline expectations before any measurement:

```yaml
TransportCharacteristics:
  expected_latency: LatencySpec           # typical latency range for this transport type
  expected_jitter: JitterSpec             # typical jitter range
  expected_loss: LossSpec                 # typical loss rate
  expected_bandwidth_bps: BandwidthSpec?  # typical bandwidth (if applicable)
  quality: InfoQuality                    # always "spec" — these are transport-class defaults
```

These are `spec` quality values — they represent what you'd typically expect
from this class of transport under normal conditions. They are used as initial
assumptions when a link is first discovered, before any measurement. Once
`measured` data is available, it overrides these baselines.

**Expected characteristics by transport type**:

| Transport | Expected latency (p50) | Expected jitter (p95) | Expected loss | Notes |
|-----------|----------------------|----------------------|---------------|-------|
| `local-pipe` | <0.1ms | <0.01ms | 0 | Kernel IPC, essentially zero |
| `pipewire` | <1ms | <0.1ms | 0 | Same-machine, kernel scheduling |
| `usb-gadget` | <1ms | <0.5ms | 0 | USB polling interval dependent |
| `v4l2` | 1–5ms | <1ms | 0 | Frame capture timing |
| `udp-direct` (wired LAN) | <0.5ms | <0.1ms | <0.001% | Switched Ethernet, near-zero jitter |
| `udp-aead` (wired LAN) | <0.5ms | <0.1ms | <0.001% | Same + encryption overhead |
| `udp-direct` (WiFi 5/6) | 1–5ms | 1–10ms | 0.1–1% | Contention, retransmits, variable |
| `udp-aead` (WiFi 5/6) | 1–5ms | 1–10ms | 0.1–1% | Same + encryption overhead |
| `wireguard` (LAN) | 0.5–1ms | <0.2ms | <0.001% | Tunnel overhead on local network |
| `wireguard` (Internet, fibre) | 5–50ms | 1–5ms | <0.1% | ISP dependent, generally stable |
| `wireguard` (Internet, cable) | 10–60ms | 2–15ms | <0.5% | DOCSIS contention, bufferbloat |
| `wireguard` (Internet, DSL) | 15–80ms | 5–20ms | <1% | Last-mile variable |
| `wireguard` (satellite, LEO) | 20–60ms | 5–30ms | 0.5–3% | Starlink: variable, weather-dependent |
| `wireguard` (satellite, GEO) | 500–700ms | 10–50ms | 1–5% | Geostationary: high but stable latency |
| `wireguard` (cellular 4G) | 20–80ms | 10–50ms | 1–5% | Highly variable, tower handoff |
| `wireguard` (cellular 5G) | 5–30ms | 2–15ms | 0.5–2% | Better than 4G, still variable |
| `bluetooth` (Classic) | 5–20ms | 2–10ms | <1% | Adaptive frequency hopping |
| `bluetooth` (BLE) | 7–30ms | 5–20ms | <2% | Connection interval dependent |
| `serial` | <1ms | <0.5ms | 0 | Point-to-point, baudrate limited |
| `websocket` (LAN) | 1–5ms | <1ms | 0 | TCP, head-of-line blocking possible |
| `websocket` (Internet) | 10–100ms | 5–30ms | <0.5% | TCP + TLS + ISP |
| `webrtc` (LAN) | 1–5ms | <1ms | <0.1% | DTLS/UDP, low overhead |
| `webrtc` (Internet) | 10–100ms | 5–30ms | 0.5–2% | STUN/TURN dependent |
| `sunshine` | 5–15ms | 1–5ms | <0.1% | Optimised for LAN game streaming |
| `mqtt` | 5–50ms | 2–20ms | <0.5% | Broker-dependent, TCP |
| `cec` | 50–200ms | 20–100ms | <1% | Slow bus, single-wire |
| `ir` | 50–200ms | 10–50ms | 1–5% | Line-of-sight, no feedback |

These values are starting points. The router refines them with measurement.
The important insight is that the router can make reasonable initial decisions
even before probing — it knows that a WiFi link will have 10–100× the jitter
of a wired LAN link, and a satellite hop will have 1000× the latency of a
local pipe. This informs path selection immediately on graph construction,
before any measurement traffic is injected.

**Path classification**: The router can also infer expected characteristics
from the network path type when a specific transport's characteristics depend
on the underlying network. A `wireguard` tunnel's jitter depends on whether
it's running over wired LAN, WiFi, fibre internet, or satellite. The transport
plugin reports the detected path class (from traceroute analysis, interface
type, or user configuration), and the router selects the matching baseline
row from the table above.

#### Bluetooth connection model

Bluetooth is a transport with highly variable characteristics depending on
the profile, codec, and connection quality. The router needs to understand
these to make correct pipeline decisions.

**Bluetooth link state**:

```yaml
BluetoothLinkState:
  profile: BluetoothProfile     # which profile is active
  codec: BluetoothCodec?        # negotiated audio codec (A2DP/LE Audio)
  connection: BluetoothConnection  # signal quality and parameters
  device: BluetoothDeviceInfo   # remote device capabilities

BluetoothProfile: enum
  a2dp_source                   # high-quality audio output (speaker, headphones)
  a2dp_sink                     # receive audio from phone/tablet
  hfp                           # hands-free call audio (bidirectional, narrowband)
  hfp_wideband                  # HFP with mSBC (16kHz wideband)
  hid                           # human interface device (keyboard, mouse, gamepad)
  le_audio_unicast              # LE Audio (LC3, low latency, bidirectional)
  le_audio_broadcast            # Auracast broadcast (one-to-many)
  ble_gatt                      # BLE data (sensors, control, beacons)
  spp                           # serial port profile (legacy data)
  pan                           # personal area network (IP over BT)

BluetoothCodec:
  name: string                  # codec identifier
  bitrate_kbps: uint?           # negotiated bitrate
  sample_rate: uint?            # Hz
  bit_depth: uint?              # bits per sample
  channels: uint?               # 1=mono, 2=stereo
  latency_ms: float?            # codec latency (encode + decode)
  lossy: bool
  quality: InfoQuality          # how we know this (reported from stack, or assumed from codec name)
```

**Bluetooth audio codecs** — what the router needs to know for format
negotiation and intent matching:

| Codec | Bitrate | Sample rate | Latency | Lossy | Notes |
|-------|---------|-------------|---------|-------|-------|
| SBC | 198–345 kbps | 44.1/48 kHz | 30–50ms | Yes | Mandatory A2DP. Worst quality, always available. |
| AAC | 256 kbps | 44.1/48 kHz | 50–80ms | Yes | Good quality. Higher latency (encoder complexity). |
| aptX | 352 kbps | 44.1/48 kHz | ~40ms | Yes | Qualcomm. CD-like quality. |
| aptX HD | 576 kbps | 48 kHz/24-bit | ~40ms | Yes | Qualcomm. Hi-res. |
| aptX Adaptive | 280–420 kbps | 48/96 kHz | 50–80ms | Yes | Qualcomm. Variable bitrate, adaptive latency. |
| aptX Lossless | ~1 Mbps | 44.1 kHz/16-bit | ~40ms | No* | Qualcomm. CD lossless (*falls back to lossy if bandwidth constrained). |
| LDAC | 330/660/990 kbps | Up to 96 kHz/24-bit | 40–60ms | Yes | Sony. Best A2DP quality at 990 kbps. |
| LC3 (LE Audio) | 16–345 kbps | 8–48 kHz | 7–10ms | Yes | Bluetooth 5.2+. Low latency. Future standard. |
| LC3plus | 16–672 kbps | Up to 96 kHz | 5–10ms | Yes | Enhanced LC3. Hi-res + low latency. |
| mSBC | 64 kbps | 16 kHz | ~10ms | Yes | HFP wideband voice. |
| CVSD | 64 kbps | 8 kHz | ~10ms | Yes | HFP narrowband voice. Legacy. |

**Impact on routing**: `fidelity_audio` intent would reject all Bluetooth
audio codecs except aptX Lossless (and even that has caveats). `desktop`
intent accepts any codec. The router checks `BluetoothCodec.lossy` against
the intent's `forbidden_formats` and `prefer_lossless` preference. The
codec's latency adds to the pipeline's total latency budget.

**Bluetooth connection quality**:

```yaml
BluetoothConnection:
  rssi_dbm: int?                # received signal strength (-30 = excellent, -90 = poor)
  tx_power_dbm: int?            # transmit power level
  link_quality: uint?           # 0–255 (controller-reported link quality)
  distance_estimate_m: float?   # estimated distance from RSSI (very approximate)
  interference: bool?           # detected interference (frequent retransmits)
  version: string?              # "4.0", "4.2", "5.0", "5.2", "5.3"
  phy: string?                  # "1m" (LE 1M), "2m" (LE 2M), "coded" (LE Coded, long range)
  mtu: uint?                    # negotiated MTU
  connection_interval_ms: float? # BLE connection interval (7.5–4000ms)
  supervision_timeout_ms: uint?  # how long before disconnect on silence

BluetoothDeviceInfo:
  name: string?                 # device name
  address: string               # MAC address
  address_type: string?         # "public", "random"
  paired: bool
  bonded: bool                  # has long-term key
  supported_profiles: BluetoothProfile[]
  supported_codecs: string[]?   # A2DP codec capabilities from device SDP/AVDTP
  battery_percent: uint?        # if reported via HFP or BLE battery service
  manufacturer: string?         # from device database or OUI
  device_db_id: string?         # matched device database entry
```

**Bluetooth RSSI feeds into the routing graph**: The signal strength is a
`measured` quality link property. If RSSI drops below -80 dBm, the router
expects increased jitter and loss. If it drops below -90 dBm, the link is
marked `degraded`. This feeds into re-evaluation triggers (§8.4) — the
router may switch to a wired path if Bluetooth quality degrades.

**Multi-device**: Bluetooth can maintain multiple simultaneous connections
(Classic + BLE, or multiple BLE). Each connection is a separate link in
the graph with its own profile, codec, and quality metrics.

#### WiFi connection model

WiFi links have highly variable characteristics depending on the standard,
band, channel conditions, client count, and interference. The router needs
real-time visibility into these to make good decisions.

**WiFi link state**:

```yaml
WiFiLinkState:
  interface: string             # OS interface name ("wlan0", "wlp2s0")
  standard: string              # "wifi4" (802.11n), "wifi5" (ac), "wifi6" (ax),
                                # "wifi6e" (ax 6GHz), "wifi7" (be)
  band: string                  # "2.4ghz", "5ghz", "6ghz"
  channel: uint                 # channel number (1–14 for 2.4GHz, 36–177 for 5GHz, etc.)
  channel_width_mhz: uint      # 20, 40, 80, 160, 320
  signal: WiFiSignalQuality     # signal strength and noise
  link_rate: WiFiLinkRate       # PHY rate and negotiated speed
  ap: WiFiAccessPointInfo?      # connected AP details (if STA mode)
  clients: uint?                # connected client count (if AP mode)
  airtime: WiFiAirtime?         # channel utilisation metrics
  roaming: WiFiRoamingState?    # roaming state (if multiple APs)

WiFiSignalQuality:
  rssi_dbm: int                 # received signal strength (-30 = excellent, -90 = barely usable)
  noise_dbm: int?               # noise floor (typically -90 to -95 dBm)
  snr_db: float?                # signal-to-noise ratio (rssi - noise)
  quality_percent: float?       # OS-reported quality (0–100)
  quality: InfoQuality          # "measured" from driver, "reported" from OS

WiFiLinkRate:
  tx_rate_mbps: float           # current transmit PHY rate
  rx_rate_mbps: float           # current receive PHY rate
  mcs_index: uint?              # MCS index (determines modulation + coding)
  spatial_streams: uint?        # MIMO spatial streams (1–8)
  guard_interval: string?       # "long" (800ns), "short" (400ns), "very_short" (800ns WiFi7)
  # Note: PHY rate ≠ throughput. Real throughput is typically 50–70% of PHY rate
  # due to protocol overhead, retransmits, and contention.
  estimated_throughput_mbps: float?  # estimated real throughput

WiFiAirtime:
  channel_utilisation_percent: float?  # how busy the channel is (0–100)
  tx_airtime_percent: float?    # our transmit airtime
  rx_airtime_percent: float?    # our receive airtime
  busy_percent: float?          # total detected busy time (includes other networks)
  # High channel utilisation = high jitter, high latency, potential packet loss
  # The router uses this to predict link quality degradation

WiFiAccessPointInfo:
  bssid: string                 # AP MAC address
  ssid: string                  # network name
  security: string?             # "wpa2", "wpa3", "open"
  ap_device_id: string?         # if this AP is an Ozma-managed access_point device

WiFiRoamingState:
  current_ap: string            # BSSID of currently connected AP
  available_aps: WiFiApCandidate[]  # other APs on same SSID with their signal levels
  last_roam: timestamp?         # when we last roamed
  roam_count: uint?             # total roams in this session
  # Frequent roaming indicates marginal coverage — the router should prefer
  # wired paths for latency-sensitive traffic on this device

WiFiApCandidate:
  bssid: string
  rssi_dbm: int
  channel: uint
  band: string
```

**WiFi quality ranges and routing implications**:

| RSSI | SNR | Quality | Expected throughput | Expected jitter | Router action |
|------|-----|---------|--------------------|-----------------|----|
| > -50 dBm | > 40 dB | Excellent | 80–100% of PHY rate | <2ms | Full bandwidth, all intents |
| -50 to -60 | 30–40 | Good | 60–80% | 2–5ms | Most intents OK; prefer wired for gaming/creative |
| -60 to -70 | 20–30 | Fair | 40–60% | 5–15ms | Degrade video quality; HID OK; audio may glitch |
| -70 to -80 | 10–20 | Poor | 20–40% | 15–50ms | HID and control only; route media over wired |
| < -80 dBm | < 10 | Unusable | Unreliable | > 50ms | Mark link as degraded; failover |

**Channel utilisation**: Even with strong signal, a congested channel
degrades performance. If `channel_utilisation_percent` > 70%, the router
treats the link as if RSSI were 10 dBm worse. This is how apartment
buildings with 30 WiFi networks on channel 6 get correctly modelled — the
signal is strong but the medium is saturated.

**Band selection awareness**: The router knows that 2.4 GHz has longer
range but less bandwidth and more interference than 5 GHz, and that 6 GHz
(WiFi 6E/7) has the most bandwidth but shortest range. If a device supports
multiple bands, the link's expected characteristics depend on which band
is active. A device on 5 GHz channel 36 at 80 MHz width has very different
properties than the same device on 2.4 GHz channel 1 at 20 MHz.

#### Serial connection model

Serial links (RS-232, RS-485, USB-serial, UART) are the primary transport
for switch control, actuator commands, serial consoles, sensor buses, and
legacy industrial equipment. Serial links have fixed parameters that
determine bandwidth and behaviour:

**Serial link state**:

```yaml
SerialLinkState:
  port: string                  # OS device path ("/dev/ttyUSB0", "/dev/ttyS0", "COM3")
  interface_type: string        # "rs232", "rs485", "uart", "usb_serial", "virtual"
  baud_rate: uint               # bits per second (300–4000000)
  data_bits: uint               # 5, 6, 7, 8
  parity: string                # "none", "even", "odd", "mark", "space"
  stop_bits: float              # 1, 1.5, 2
  flow_control: string          # "none", "rts_cts" (hardware), "xon_xoff" (software)
  protocol: SerialProtocol?     # application-level protocol running on this link
  usb_path: string?             # for USB-serial: USB bus path ("1-2.3")
  usb_chipset: string?          # USB-serial chipset ("FTDI FT232R", "CP2102", "CH340",
                                # "PL2303", "Silabs CP2104")
  usb_vid_pid: string?          # USB VID:PID of the adapter
  persistent_id: string?        # udev persistent path or serial number (for stable identification)

SerialProtocol: enum
  raw                           # raw byte stream (serial console, custom protocols)
  modbus_rtu                    # Modbus RTU (RS-485, CRC16, request/response)
  modbus_ascii                  # Modbus ASCII
  dmx512                        # DMX lighting control (250 kbaud, RS-485)
  midi_din                      # MIDI over DIN (31.25 kbaud)
  nmea                          # GPS NMEA sentences
  at_commands                   # Hayes AT command set (modems, some IoT modules)
  custom                        # device-specific protocol (TESmart, Extron, etc.)
```

**Effective bandwidth**: Serial bandwidth is `baud_rate / (data_bits +
parity_bits + stop_bits + start_bit)` bytes per second. At 115200 8N1,
that's 11,520 bytes/sec — plenty for control commands, inadequate for media.

| Baud rate | Effective throughput | Typical use |
|-----------|---------------------|-------------|
| 9600 | 960 B/s | Legacy devices, some switches, Modbus sensors |
| 19200 | 1,920 B/s | Industrial equipment, some actuators |
| 38400 | 3,840 B/s | Faster control protocols |
| 57600 | 5,760 B/s | Serial consoles (default on many SBCs) |
| 115200 | 11,520 B/s | Serial consoles, USB-serial default, most modern devices |
| 250000 | 25,000 B/s | DMX512 (fixed at 250 kbaud) |
| 921600 | 92,160 B/s | Fast USB-serial, firmware upload |
| 1000000+ | 100,000+ B/s | Direct UART (no USB-serial overhead) |

**USB-serial vs native UART**: USB-serial adapters add latency from the USB
polling interval. A typical USB-serial adapter at full-speed USB has a 1ms
polling interval — every byte waits up to 1ms before being delivered to the
host. At high-speed USB this drops to 125µs. Native UART (direct SBC GPIO
pins) has no USB overhead — latency is just wire propagation + one bit time.

| Interface | Added latency | Notes |
|-----------|--------------|-------|
| Native UART (SBC GPIO) | <0.1ms | Direct hardware, no stack overhead |
| USB-serial (full-speed) | 1–2ms | USB polling interval + driver |
| USB-serial (high-speed) | 0.1–0.5ms | Faster polling + driver |
| USB-serial via hub | 1–5ms | Hub adds another polling stage |
| Bluetooth SPP | 10–30ms | BT Classic serial port profile |
| WiFi serial bridge | 5–20ms | ESP32/ESP8266 WiFi-to-serial |
| Virtual serial (socat/pty) | <0.1ms | Software pipe, kernel IPC |

**Persistent identification**: Serial ports change names across reboots
(`/dev/ttyUSB0` might become `/dev/ttyUSB1`). The spec requires stable
identification — `persistent_id` uses udev `by-id` or `by-path` symlinks,
or USB serial number. A device database entry for a USB-serial adapter
includes its VID/PID and chipset, enabling automatic re-identification.

**RS-485 specifics**: RS-485 is multi-drop (multiple devices on one bus)
with half-duplex communication. The serial link model includes:

```yaml
Rs485Config:
  mode: string                  # "half_duplex" (standard), "full_duplex" (4-wire)
  termination: bool             # bus termination enabled
  address: uint?                # device address on the bus (Modbus: 1–247)
  max_devices: uint?            # maximum devices on this bus (RS-485: 32 standard, 256 with repeaters)
  turnaround_ms: float?         # minimum time between TX and RX (bus direction change)
```

RS-485 buses are modelled as a single link with multiple devices — the bus
is a shared medium, like WiFi. Bandwidth is shared among all devices. The
router knows not to expect full throughput when multiple devices are active.

**Serial as control path**: Most serial links in Ozma carry control commands,
not media data. A TESmart matrix switch connected via USB-serial at 9600 baud
needs a few bytes per command — the bandwidth is irrelevant, but the latency
matters for pipeline activation (§2.6). The serial link's activation time
is essentially the command round-trip: send command + wait for response (if
the device confirms) or send command + assume success (if write-only).

**Serial console as data stream**: A serial console (BIOS output, bootloader
logs, kernel messages) is a data source. The serial link carries text at
whatever baud rate is configured. The node captures this via
`NodeSerialCapture` and exposes it as a data port in the graph. The text
can be displayed in the dashboard terminal view or processed by OCR triggers.

#### Constrained and exotic transports

Not every transport carries high-bandwidth media. Some transports are
extremely low-bandwidth but serve important functions — sensor data,
control commands, presence detection, or telemetry from remote locations.
The routing graph models these with the same primitives but different
characteristic profiles.

**Constrained transport characteristics**:

| Transport | Bandwidth | Range | Latency | Use in Ozma |
|-----------|-----------|-------|---------|-------------|
| LoRa | 0.3–50 kbps | 2–15 km | 100ms–5s | Remote sensor data, presence, alerts from distant buildings/farms |
| Zigbee | 250 kbps | 10–100m (mesh) | 15–30ms | IoT sensors, door contacts, motion detectors |
| Z-Wave | 100 kbps | 30–100m (mesh) | 15–30ms | IoT sensors, locks, thermostats |
| Thread/Matter | 250 kbps | 10–100m (mesh) | 10–30ms | IP-based IoT mesh (newer devices) |
| Sub-GHz (433/868/915 MHz) | 1–100 kbps | 0.5–5 km | 50ms–1s | Custom sensors, weather stations, gate controllers |
| Power line (HomePlug/G.hn) | 50–2000 Mbps | Same circuit | 5–30ms | Networking through existing wiring |
| IrDA | 9.6–16000 kbps | <1m, line of sight | <5ms | Legacy data transfer |
| NFC | 424 kbps | <10cm | <1ms | Badge/tag read for workspace profiles |
| UWB | 6.8–27.2 Mbps | 10–30m | <1ms | Precise positioning (~10cm accuracy) |

**Constrained transports as plugins**: These follow the same transport
plugin contract (§6.1). A LoRa plugin discovers LoRa gateways and devices,
reports links with appropriate characteristics (50 kbps, 2s latency), and
opens/closes data streams. The router knows not to route video over LoRa
because the bandwidth constraint (§8.2) eliminates it — but it happily
routes sensor data or control commands.

**LoRa-specific model**:

```yaml
LoRaLinkState:
  spreading_factor: uint        # SF7–SF12 (higher = longer range, lower bitrate)
  bandwidth_khz: uint           # 125, 250, 500
  coding_rate: string           # "4/5", "4/6", "4/7", "4/8"
  frequency_mhz: float         # operating frequency (868.1, 915.0, etc.)
  tx_power_dbm: int             # transmit power
  rssi_dbm: int                 # received signal strength
  snr_db: float                 # signal-to-noise ratio
  airtime_ms: float?            # last packet airtime
  duty_cycle_percent: float?    # regulatory duty cycle limit (1% in EU 868MHz)
  gateway: LoRaGatewayInfo?     # which gateway received this

LoRaGatewayInfo:
  id: string
  location: PhysicalLocation?
  type: string                  # "single_channel", "8_channel", "lorawan_gateway"
  network: string?              # "private", "ttn" (The Things Network), "helium", "chirpstack"
```

**Use cases for constrained transports in Ozma**:

- **Remote building sensors**: A farm outbuilding with a LoRa temperature/
  humidity sensor → gateway on the controller's building → sensor device in
  the graph → monitoring dashboard + trend alerts + automation triggers.
  No WiFi needed at the outbuilding.

- **Gate/door status**: Sub-GHz contact sensor on a driveway gate, 500m from
  the house → received by controller with sub-GHz radio → event triggers
  doorbell alert or security scenario.

- **Workspace presence via NFC**: Tap NFC badge at desk → workspace profile
  activates → scenarios, audio routing, screen layout all switch. The NFC
  read is a near-zero-latency, near-zero-bandwidth transport that carries
  identity data.

- **UWB positioning**: UWB anchors in a room provide ~10cm positioning
  accuracy → feeds `UserZone` (§8.1) with `measured` quality position data →
  spatial routing decisions based on actual position, not inferred from
  which keyboard is active.

- **Power line networking**: A node in a garage that can't be wired with
  Ethernet but is on the same electrical circuit → HomePlug adapter gives
  50–200 Mbps, enough for KVM video. The transport characteristics table
  gives it appropriate jitter/latency expectations.

**Built-in transports** (shipped with Ozma):

| Transport ID | Description |
|-------------|-------------|
| `udp-direct` | Raw UDP, no encryption. LAN only. |
| `udp-aead` | UDP with XChaCha20-Poly1305 per-packet encryption. Default for LAN. |
| `wireguard` | WireGuard tunnel. For remote access, Connect relay. |
| `pipewire` | PipeWire link (audio, same machine). |
| `local-pipe` | Unix domain socket or pipe (same machine). |
| `usb-gadget` | USB gadget interface (HID, audio, video via configfs). |
| `v4l2` | V4L2 device interface (capture card → userspace). |
| `bluetooth` | Bluetooth Classic (A2DP, HFP) and BLE. Audio and control. |
| `serial` | RS-232 / USB-serial. Control surfaces, switches, actuators, serial consoles. |
| `websocket` | WebSocket (TCP). Browser displays, remote desktop, screen server. |
| `webrtc` | WebRTC (UDP/TCP). Browser-based video/audio/HID with DTLS. |
| `sunshine` | Sunshine/Moonlight game streaming session (RTSP + RTP + control). |
| `qmp` | QEMU Machine Protocol. VM control (input injection, power, display). |
| `mqtt` | MQTT pub/sub. IoT devices, sensors, actuators, doorbell events. |
| `cec` | HDMI CEC (pin 13). Display power/input control, switch control. |
| `ddc-ci` | DDC/CI (I2C over display cable). Monitor brightness, power, input. |
| `ir` | Infrared blaster/receiver. Write-only switch control. |
| `vban` | VBAN protocol (UDP). Uncompressed audio, established open standard. |
| `hid-usb` | USB HID reports. Vendor-specific device control (some switches, RGB). |

**Example exotic transports** (not shipped, but the contract allows them):

| Transport ID | Description |
|-------------|-------------|
| `hdmi-loopback` | HDMI output → HDMI capture card on another machine. |
| `ndi` | NDI network video (discovery, transport, format). |
| `dante` | Dante/AES67 audio network. |
| `usb-ip` | USB/IP forwarding. |
| `rtsp` | RTSP client/server (IP cameras, re-publishing). |
| `onvif` | ONVIF camera discovery, control, and PTZ. |
| `looking-glass` | IVSHMEM shared memory (zero-copy VM display from VFIO). |
| `osc` | Open Sound Control (UDP). Network control surfaces. |
| `kdeconnect` | KDE Connect protocol. Phone integration (notifications, media, clipboard). |
| `nut` | Network UPS Tools protocol. UPS monitoring. |
| `wol` | Wake-on-LAN magic packets. Target machine power-on. |
| `lora` | LoRa/LoRaWAN. Remote sensors, gate status, farm buildings. |
| `zigbee` | Zigbee mesh. IoT sensors, contacts, motion. |
| `zwave` | Z-Wave mesh. IoT sensors, locks, thermostats. |
| `thread` | Thread/Matter mesh. IP-based IoT. |
| `sub-ghz` | Custom sub-GHz radio (433/868/915 MHz). Long-range sensors. |
| `powerline` | HomePlug/G.hn. Networking through electrical wiring. |
| `nfc` | NFC tag/badge read. Workspace profile activation. |
| `uwb` | Ultra-wideband. Precise indoor positioning (~10cm). |

### 6.2 Device plugin

A device plugin makes a class of hardware or software discoverable to the graph.

```yaml
DevicePlugin:
  id: string                    # "v4l2", "usb", "thunderbolt", "pipewire-audio",
                                # "alsa", "network", "virtual-display"
  name: string
  platforms: Platform[]         # which OS platforms this plugin works on

  # --- Methods ---

  discover():
    # Finds all devices of this type on the local machine.
    # Returns device descriptors with ports and capabilities.
    returns: Device[]

  get_topology(device: DeviceRef):
    # Returns internal structure of a compound device.
    # For a USB hub: which ports connect to which controller.
    # For a Thunderbolt dock: internal hub, ethernet, display, audio topology.
    returns: DeviceTopology { sub_devices: Device[], internal_links: Link[] }

  on_hotplug(callback):
    # Notifies when devices of this type appear or disappear.

  get_properties(device: DeviceRef):
    # Returns device-specific properties and capabilities.
    # Properties are InfoQuality-tagged.
    returns: PropertyBag
```

**Platform-specific discovery**:

| Platform | USB topology | Thunderbolt | Audio | Display |
|----------|-------------|-------------|-------|---------|
| Linux | `lsusb -t`, `udevadm`, sysfs | `boltctl` | PipeWire, ALSA | DRM/KMS, xrandr |
| Windows | SetupDi, WMI, devcon | Intel SDK | WASAPI, WMI | DXGI, WMI |
| macOS | `system_profiler SPUSBDataType` | `system_profiler SPThunderboltDataType` | CoreAudio | CoreGraphics |

Each platform reports what it can. Unknown properties get `assumed` quality.

### 6.3 Codec plugin

A codec plugin handles encoding and decoding of media data.

```yaml
CodecPlugin:
  id: string                    # "ffmpeg-h264", "vaapi-h265", "nvenc-h264",
                                # "opus-encoder", "pcm-passthrough"
  name: string
  type: encoder | decoder | both
  media_type: video | audio
  hardware: bool                # is this a hardware-accelerated codec?
  platform: string?             # hardware identifier ("intel-qsv", "amd-vcn", "nvidia-nvenc")

  # --- Methods ---

  supported_formats():
    # Returns pairs of (input_format, output_format) this codec can handle.
    returns: FormatPair[]

  estimated_latency(input: Format, output: Format):
    # Returns expected encode/decode latency for this format pair.
    returns: LatencySpec

  estimated_quality(input: Format, output: Format):
    # Returns expected quality metrics (PSNR, SSIM) if known.
    returns: QualityEstimate?

  create_transcoder(input: Format, output: Format):
    # Creates an encoder/decoder instance.
    returns: TranscoderHandle

  measure_performance(input: Format, output: Format):
    # Benchmarks actual encode/decode performance.
    # Used to get measured (not estimated) latency.
    returns: CodecMetrics { latency, throughput, cpu_usage, gpu_usage }
```

### 6.4 Converter plugin

A converter transforms data between formats without being a full codec. Examples:
pixel format conversion, sample rate conversion, channel remixing, HID report
translation.

```yaml
ConverterPlugin:
  id: string                    # "pixel-format", "resample", "channel-remix",
                                # "hid-qmp", "hid-gadget"
  name: string
  media_type: video | audio | hid | data

  # --- Methods ---

  supported_conversions():
    returns: FormatPair[]

  estimated_latency(input: Format, output: Format):
    returns: LatencySpec

  is_lossless(input: Format, output: Format):
    # Returns true if this conversion preserves all information.
    returns: bool

  create(input: Format, output: Format):
    returns: ConverterHandle
```

### 6.5 Switch plugin

A switch plugin controls an external switching device (KVM switch, HDMI matrix,
audio matrix, AV receiver, etc.). Switch plugins bridge between the graph model
and the device's control interface.

```yaml
SwitchPlugin:
  id: string                    # "tesmart-serial", "hdmi-cec", "extron-ip",
                                # "ir-blaster", "manual"
  name: string
  media_types: MediaType[]      # what this switch routes (video, audio, hid, mixed)
  controllability: Controllability  # see §2.5

  # --- Methods ---

  discover():
    # Finds switches this plugin can control.
    # For IP-based switches: network scan or configured addresses.
    # For serial: enumerate serial ports, probe for known protocols.
    # For CEC: enumerate HDMI-CEC devices.
    # For manual: returns user-configured switch definitions.
    returns: Device[]

  get_matrix(device: DeviceRef):
    # Returns the current routing matrix.
    # For confirmed devices: queries the device.
    # For write-only devices: returns last commanded state.
    # For manual devices: returns last user-reported state.
    returns: SwitchMatrix

  set_route(device: DeviceRef, input_port: PortRef, output_port: PortRef):
    # Commands the switch to connect input to output.
    # For write-only devices: sends the command, returns commanded quality.
    # For confirmed devices: sends command, reads back state, returns reported quality.
    # For manual devices: returns an error (cannot command).
    returns: { success: bool, state_quality: InfoQuality }

  on_state_change(device: DeviceRef, callback):
    # For devices that emit state change events (confirmed or event-only).
    # Callback receives the new SwitchMatrix.
    # For write-only and manual devices: no-op (no events available).
```

**Control interface examples**:

| Interface | Protocol | Feedback | Typical devices |
|-----------|----------|----------|-----------------|
| Serial (RS-232/USB) | Device-specific command set | Varies — some echo state, some are silent | TESmart, Extron, Crestron, many pro AV |
| IP (TCP/UDP) | HTTP API, Telnet, or proprietary | Usually confirmed | Extron, Blackmagic, enterprise AV |
| IR blaster | Consumer IR codes | Write-only — no feedback channel | Cheap HDMI switches, AV receivers |
| HDMI CEC | CEC protocol over HDMI pin 13 | Confirmed (CEC has acknowledgement) | TVs, AV receivers, some monitors |
| USB HID | Vendor-specific HID reports | Varies by device | Some USB KVM switches |
| Manual | N/A | User confirmation only | Physical button switches |

**Write-only devices and verification**: When the router activates a pipeline
through a write-only switch, it cannot confirm the switch state directly. But
it can verify indirectly: if video or audio starts flowing through the expected
path after the switch command, the state is implicitly confirmed and upgraded
from `commanded` to `measured`. If no data flows within a timeout, the router
treats the switch state as `assumed` (possibly wrong) and may retry or alert.

---

## 7. Clock Model

Devices in the Ozma graph have independent clocks. Unlike PipeWire (where a
single driver clock governs each subgraph), Ozma must synchronise across
network boundaries.

### 7.1 Clock domains

A **clock domain** is a set of devices that share a common time reference. On a
single machine, all devices can share the system clock. Across machines, clocks
diverge.

```yaml
ClockDomain:
  id: string
  reference: ClockReference     # what this domain synchronises to
  members: DeviceRef[]          # devices in this domain
  drift_ppm: float?             # measured clock drift if known
```

**ClockReference**:

```yaml
ClockReference: enum
  system_monotonic   # local monotonic clock (CLOCK_MONOTONIC)
  ptp                # IEEE 1588 Precision Time Protocol
  ntp                # NTP-synchronised wall clock
  audio_device       # locked to an audio device's word clock
  free_running       # no synchronisation (each device runs independently)
```

### 7.2 Synchronisation strategy

**Same machine**: All devices share `system_monotonic`. No synchronisation
needed. Latency measurements are directly comparable.

**LAN (same broadcast domain)**: PTP (IEEE 1588v2) provides sub-microsecond
synchronisation on standard Ethernet — no proprietary hardware required. This
is the same clock mechanism used by Dante, AES67, and SMPTE ST 2059. The
controller runs a PTP grandmaster; nodes synchronise to it.

PTP accuracy depends on hardware timestamping support:

| NIC capability | PTP accuracy | Audio quality achievable | Hardware examples |
|---------------|-------------|------------------------|-------------------|
| Hardware timestamps (PHC) | <1µs | Sample-accurate at 192kHz (1 sample = 5.2µs) | Intel I210, I225, I226; Broadcom BCM5720; Realtek RTL8125B (some) |
| Software timestamps only | 10–100µs | Sample-accurate at 48kHz (1 sample = 20.8µs); sub-sample at 96kHz | Most USB Ethernet, WiFi adapters, budget NICs |
| NTP only (no PTP) | 1–10ms | Adequate for KVM audio; not for pro audio sync | Any network interface |

The controller detects PTP hardware timestamp support via `ethtool -T` (Linux)
and selects the best available clock source automatically. Hardware PTP is
preferred; software PTP is the fallback; NTP is the last resort. The
`InfoQuality` on the clock sync reflects this: hardware PTP = `measured`,
software PTP = `measured` (lower confidence), NTP = `reported`.

**QoS marking**: Audio packets are marked with DSCP EF (Expedited Forwarding,
value 46) to receive priority treatment on managed switches. This is the same
QoS classification used by Dante and AES67. The transport plugin sets the
DSCP value on the socket (`setsockopt IP_TOS`). On unmanaged switches, DSCP
has no effect but causes no harm.

**Latency classes**: Like Dante's selectable latency, Ozma's audio transport
offers configurable jitter buffer depth that determines the latency/reliability
trade-off:

| Jitter buffer | One-way latency added | Tolerance | Use case |
|--------------|----------------------|-----------|----------|
| 0.25ms (12 samples @ 48k) | 0.25ms | Very tight — requires PTP + dedicated/managed switch | Local monitoring |
| 0.5ms (24 samples) | 0.5ms | Tight — PTP recommended | Studio recording |
| 1ms (48 samples) | 1ms | Moderate — software PTP sufficient | Live performance |
| 2ms (96 samples) | 2ms | Comfortable — works on any decent LAN | General pro audio |
| 5ms (240 samples) | 5ms | Relaxed — tolerates WiFi jitter | Non-critical audio |
| 20ms (960 samples) | 20ms | Very relaxed — tolerates internet jitter | Remote audio |

The intent's `max_latency_ms` constraint determines which buffer depth is
acceptable. The router selects the smallest buffer that the measured link
jitter can sustain — if the link has 0.3ms p99 jitter, a 0.5ms buffer works;
if jitter is 2ms, the buffer must be ≥2ms.

**Dante-equivalent on commodity hardware**: On a wired Gigabit LAN with a
managed switch (QoS enabled) and a NIC with hardware PTP timestamps, Ozma
achieves the same sync accuracy and latency as Dante — without Audinate
licensing, without proprietary chipsets, without vendor lock-in. The
difference is purely protocol: Dante is a closed standard; Ozma's audio
transport is open and interoperable with AES67 (which Dante itself
supports as a compatibility mode).

**Remote (via Connect relay)**: NTP synchronised to a common reference. Clock
offset is measured during session establishment and applied to latency
calculations. Accuracy is lower (tens of milliseconds) but sufficient for remote
desktop intents.

### 7.3 Drift compensation

When audio crosses clock domains, sample rate drift causes buffer underrun or
overrun. Compensation strategies:

| Strategy | Latency impact | Quality impact | Use case |
|----------|---------------|----------------|----------|
| Adaptive resampling | +1ms | Inaudible (SRC quality) | Default for cross-machine audio |
| Buffer padding | +5–20ms | None | High-fidelity where resampling is unacceptable |
| Drop/duplicate samples | 0ms | Audible clicks on drift | Emergency fallback only |

The router selects compensation strategy based on the intent:
- `fidelity_audio`: buffer padding (never resample)
- `gaming` / `desktop`: adaptive resampling
- `preview` / `observe`: not applicable (audio not required or low priority)

### 7.4 Video timing

Video frame timing across clock domains uses a simpler model: frames are
timestamped at source and played at destination with a jitter buffer. The
jitter buffer depth is derived from the intent's latency budget minus the
transport latency:

```
jitter_buffer_ms = intent.max_latency_ms - transport.latency_ms - codec.latency_ms
```

If the result is negative, the intent cannot be satisfied on this path.

---

## 8. Route Calculation

The router takes the graph (devices, ports, links), a source, a destination, and
an intent, and produces the optimal pipeline.

### 8.1 Cost model

Each link in the graph has a computed cost. The router finds the lowest-cost path
that satisfies all constraints.

**Cost function**:

```
cost(link) = w_latency × latency_ms
           + w_hops × 1
           + w_conversions × (1 if format change else 0)
           + w_bandwidth × (1 - available_bps / required_bps)  # penalise tight fits
           + w_quality_loss × quality_loss_factor
           + w_uncertainty × (1 - trust_factor(info_quality))
           + w_pressure × max(device_pressure(d) for d in devices_on_hop)  # see §2.7
```

Where `device_pressure(d)` is the highest resource utilisation ratio across all
resource types on device `d` after accounting for this pipeline's cost:

```
device_pressure(d) = max(
  (d.resource[r].current_usage + pipeline.cost[r]) / d.resource[r].capacity
  for r in resource_types
)
```

This naturally load-balances: a hop through a busy device costs more than the
same hop through an idle one.

Weights (`w_*`) are derived from the intent's preferences:

| Preference | Affects weights |
|-----------|----------------|
| `prefer_lower_latency` | Increases `w_latency` |
| `prefer_fewer_hops` | Increases `w_hops` |
| `prefer_higher_quality` | Increases `w_quality_loss`, decreases `w_conversions` tolerance |
| `prefer_hardware_codec` | Reduces cost of hardware codec hops |
| `prefer_local_zone` | Increases `w_zone` (see Spatial Zones below) |

**Spatial zones** (optional cost factor):

Devices can be assigned to named spatial zones representing physical areas.
Zones have types that inform routing and automation behaviour:

```yaml
SpatialZone:
  id: string                    # "desk-main", "couch", "conference-table", "server-closet"
  name: string                  # human-readable
  type: ZoneType                # what kind of space this is
  space: string?                # parent space/room ("study", "living_room")
  site: string?                 # parent site ("home", "office-hq")
  devices: DeviceRef[]          # devices in this zone
  furniture: FurnitureRef[]     # furniture in this zone (§2.11)
  adjacent_zones: string[]?     # zone IDs that are physically adjacent
  bounds: ZoneBounds?           # physical extent (for spatial effects, presence detection)

ZoneType: enum
  workstation                   # primary work area (desk, monitors, input devices)
  collaboration                 # meeting/conference area (shared displays, cameras, mics)
  media                         # media consumption area (couch, TV, speakers)
  server                        # infrastructure area (server rack, networking gear)
  utility                       # utility area (printer, storage, supplies)
  common                        # shared/transit area (hallway, kitchen)
  outdoor                       # outdoor area (patio, garden — cameras, sensors, lighting)

ZoneBounds:
  shape: string                 # "rectangle", "polygon", "circle"
  points: Position2d[]?         # floor-plan polygon vertices (for rectangle/polygon)
  center: Position2d?           # for circle
  radius_mm: float?             # for circle
  floor_z_mm: float?            # floor height (for multi-level)
  ceiling_z_mm: float?          # ceiling height

UserZone:
  current_zone: string?         # which zone the user is currently in (null = unknown)
  confidence: InfoQuality       # how we know
  detection_source: string?     # what determined the zone ("active_keyboard", "ble_beacon",
                                # "motion_sensor", "manual", "idle_timeout")
  zone_type: ZoneType?          # type of the current zone (cached for quick routing decisions)
```

The user's current zone is inferred from observable signals — which input
device is active (wired keyboard = desk, wireless keyboard = couch), which
motion sensor fired, or explicit user selection. When a zone is known, the
cost model applies a zone distance penalty:

```
cost(hop) += w_zone × zone_distance(user_zone, device_zone)
```

Where `zone_distance` is 0 for same zone, 1 for adjacent zones, 2+ for
distant zones. This makes the router prefer devices physically near the
user — if the user is at the couch, the TV gets a lower cost than the desk
monitor. Zone definitions and adjacency are user-configured.

The cost model naturally produces the behaviour described in §3: multi-hop paths
are possible but expensive (each hop adds latency cost + hop cost + potential
conversion cost), so the router avoids them unless no direct path exists.

### 8.2 Constraint satisfaction

Before cost ranking, candidate pipelines are filtered by hard constraints:

1. **Latency**: Sum of all hop latencies must be ≤ `max_latency_ms`
2. **Activation time**: Pipeline activation time must be ≤ `max_activation_time_ms`
   (accounts for current hop states — warm hops contribute near-zero, see §2.6)
3. **Bandwidth**: Every link must have `available_bps ≥ required_bps` for the
   negotiated format
4. **Device capacity**: Every device touched by the pipeline must have sufficient
   resources to support the pipeline's cost, including peak activation cost (§2.7).
   If any resource on any device would exceed capacity, the pipeline is rejected.
5. **Resource budget**: Every device with a resource budget must not exceed its hard
   limits after adding this pipeline's cost (§2.7). Devices in `adaptive` mode may
   pass this check with a degraded pipeline configuration.
6. **Power budget**: Every voltage rail on every device in the pipeline must have
   sufficient current headroom for the pipeline's power cost (§2.10). RGB
   pipelines are checked against LED power calculations.
7. **Loss**: Every link must have `loss_rate ≤ max_loss`
8. **Jitter**: Every link must have `jitter_p99 ≤ max_jitter_ms`
9. **Format**: At least one format in the negotiated intersection must not be
   in `forbidden_formats` and (if specified) must be in `required_formats`
10. **Hops**: Pipeline length must be ≤ `max_hops`
11. **Conversions**: Number of format changes must be ≤ `max_conversions`
12. **Encryption**: If `required`, every link must support encryption

Any pipeline that violates a hard constraint is discarded. If no pipeline
satisfies all constraints, the degradation policy is applied (§3.4).

### 8.3 Path computation

The router uses a modified Dijkstra's algorithm over the graph:

1. Build the graph from discovered devices, ports, and links
2. For each candidate source→destination pair:
   a. Enumerate all paths (bounded by `max_hops` or a reasonable default)
   b. For each path, compute format negotiation (§4.3)
   c. If format negotiation fails (empty intersection), try inserting converters
   d. Apply constraint filter
   e. Compute cost for surviving paths
3. Select the lowest-cost path
4. Fixate formats on each link
5. Return the Pipeline

**Converter insertion**: When two adjacent ports have incompatible formats, the
router searches for a converter plugin that bridges them. Each converter is
modelled as a virtual device with a sink port (input format) and source port
(output format). Inserting a converter adds a hop with its own latency cost.
The router tries all available converters and picks the one with lowest cost.

### 8.4 Re-evaluation triggers

Pipelines are re-evaluated when the graph changes. Changes that trigger
re-evaluation:

| Trigger | Source | Response |
|---------|--------|----------|
| Device added/removed | Hotplug event from device plugin | Full re-evaluation of affected pipelines |
| Link metrics changed | Periodic measurement, passive monitoring | Re-evaluate if metrics cross constraint boundaries |
| Link failed | Transport plugin reports failure | Immediate failover to next-best pipeline |
| Intent changed | User action, scenario switch, automation | Full re-evaluation with new intent |
| Bandwidth contention | Measured available bandwidth dropped | Re-evaluate affected pipelines, may degrade |
| Device resource pressure | CPU/memory/GPU usage approaching limits (§2.7) | Degrade or cool warm pipelines on pressured device |
| Budget exceeded | Agent reports usage above backoff threshold (§2.7) | Reduce pipeline quality on that device |
| Power rail pressure | Voltage drop or current approaching rail capacity (§2.10) | Scale RGB brightness, cool warm pipelines, alert |
| External event | Meeting started (meeting_detect.py), Zoom call detected | Proactive re-evaluation with updated bandwidth expectations |

**Reactive** triggers (link failure, metric threshold crossing) cause immediate
re-evaluation. **Proactive** triggers (periodic measurement, external events)
cause background re-evaluation — the new pipeline is computed but not activated
until the current one actually degrades or the proactive assessment shows a
better path.

**Predictive** triggers (meeting detection, application launch) pre-compute
alternative pipelines. Example: a Zoom call is about to start → the router
pre-computes a pipeline that uses less bandwidth for the KVM session, ready to
swap instantly when contention is detected.

### 8.5 Switching and activation

Pipeline switching has two distinct time components (see §2.6):

1. **Route computation** — deciding which pipeline to use. This is always
   pre-computed at graph-change time, not at switch time. Cost: <1ms.

2. **Pipeline activation** — bringing the selected pipeline's hops from their
   current state to `active`. This is where real time is spent, and it varies
   enormously by component:

| Pipeline state at switch time | Activation time | How |
|-------------------------------|-----------------|-----|
| Warm (all hops initialised) | <10ms | Just start data flow |
| Partially warm (some hops cold) | Slowest cold hop | Warm cold hops in parallel where possible |
| Fully cold (nothing running) | Sum of critical path | Sequential where dependencies exist |

The router does NOT perform format negotiation, path computation, or measurement
at switch time. All of that happened earlier. Switching is activating a
pre-computed pipeline — the cost is purely the activation time of its hops.

**Warm pipeline switching** is the fast path. When the router keeps pipelines
warm for likely scenario switches (§2.6), activation is near-instantaneous:
HID redirect (<5ms) + video/audio already flowing into discard (<1ms to
redirect to real output).

**Cold pipeline switching** is honest about its cost. If switching requires an
HDMI matrix change (2s for HDCP) and ffmpeg startup (3s), the router reports
a 3–5s activation time. The user sees this in the UI and can choose to keep
that pipeline warm, accept the delay, or restructure the path.

**Activation time as a routing input**: The intent's `max_activation_time_ms`
constraint filters candidate pipelines by activation time. A `gaming` intent
might set `max_activation_time_ms: 500`, which forces the router to either
select a pipeline that's already warm or reject paths with slow components
(effectively requiring pre-warming for the gaming scenario).

The cost model (§8.1) can optionally include activation time as a factor:

```
cost(link) += w_activation × activation_time_ms  # if intent cares about fast switching
```

This penalises paths through slow-switching devices, making the router prefer
direct paths over paths through external switches when activation time matters.

### 8.6 Remediation

When a link or device fails, the router's first response is to failover to
an alternative pipeline (§8.4). But the router can also attempt to **fix**
the broken path so it becomes available again. Remediation is the model for
expressing what corrective actions are possible, how safe they are, and when
to attempt them.

**Remediation capabilities on devices and links**:

Every device and link can advertise what remediation actions it supports:

```yaml
RemediationCapability:
  action: string                # action identifier
  target: string                # "device", "link", "port", "service"
  safety: RemediationSafety     # how risky is this action
  disruption: DisruptionLevel   # what gets disrupted
  estimated_duration_ms: uint   # how long the action takes
  success_rate: float?          # historical success rate (0.0–1.0, if known)
  cooldown_ms: uint?            # minimum time between attempts
  max_attempts: uint?           # maximum retry count before escalating
  prerequisites: string[]?      # conditions that must be true (e.g., "pipeline_not_active")

RemediationSafety: enum
  safe                          # no risk — can always attempt (process restart, cache clear)
  disruptive                    # disrupts this device's active pipelines (USB rebind, service restart)
  destructive                   # affects other devices or loses state (hub power cycle, node reboot)
  manual                        # requires human action (replace cable, reseat card)

DisruptionLevel: enum
  none                          # transparent to user
  momentary                     # <1s interruption
  brief                         # 1–10s interruption
  extended                      # >10s interruption, user will notice
  full                          # device goes offline entirely
```

**Standard remediation actions**:

| Action | Target | Safety | Disruption | Description |
|--------|--------|--------|-----------|-------------|
| `process_restart` | service | safe | momentary | Restart a software service (ffmpeg, audio bridge) |
| `usb_rebind` | device | disruptive | brief | Unbind/rebind USB device via sysfs |
| `usb_hub_power_cycle` | device | destructive | extended | Power cycle a USB hub (affects all devices on hub) |
| `v4l2_reopen` | port | safe | momentary | Close and reopen V4L2 device |
| `pipewire_reconnect` | link | safe | momentary | Destroy and recreate PipeWire link |
| `network_interface_restart` | link | disruptive | brief | ifdown/ifup on network interface |
| `wifi_channel_scan` | link | disruptive | brief | Trigger WiFi channel re-evaluation |
| `node_reboot` | device | destructive | full | Reboot the entire node |
| `target_power_cycle` | device | destructive | full | LoM power cycle of target machine |
| `cable_reseat` | link | manual | full | User physically reseats a cable |
| `device_replace` | device | manual | full | User replaces faulty hardware |

**Remediation policy**:

```yaml
RemediationPolicy:
  mode: string                  # "auto", "confirm", "notify", "disabled"
  auto_actions: RemediationSafety[]  # which safety levels to auto-attempt
                                     # (default: ["safe"] — only safe actions are automatic)
  confirm_actions: RemediationSafety[]  # which to attempt after user confirmation
  notify_actions: RemediationSafety[]   # which to only notify about
  escalation_chain: EscalationStep[]    # what to do when remediation fails

EscalationStep:
  after_attempts: uint          # escalate after this many failed attempts
  action: string                # next action to try, or "alert"
  delay_ms: uint                # wait this long before escalating
```

Default policy: `safe` actions are automatic, `disruptive` actions require
confirmation, `destructive` actions are notify-only, `manual` actions
generate a recommendation. This matches the agent approval model (§ agent
engine) — the same safety philosophy applied to infrastructure.

### 8.7 Intent bindings

Intents (§3) define what the user wants. Triggers (spec 10) detect what's
happening. Intent bindings connect them — when a condition is observed,
automatically apply an intent to affected pipelines.

```yaml
IntentBinding:
  id: string
  name: string                  # human-readable ("Gaming mode on game launch")
  conditions: BindingCondition[]
  condition_mode: string        # "all" (AND) or "any" (OR)
  intent: string                # intent name to apply when conditions match
  scope: BindingScope           # which pipelines are affected
  revert: RevertPolicy          # what happens when conditions stop matching
  priority: uint                # when multiple bindings match, highest priority wins
  enabled: bool

BindingCondition:
  source: string                # what to observe
  field: string                 # which field on the source
  op: string                    # "eq", "neq", "gt", "lt", "in", "contains", "matches"
  value: any                    # comparison value

BindingScope:
  target: string                # "all", "node", "device", "pipeline"
  target_id: string?            # specific node/device/pipeline (null = all matching)
  streams: string[]?            # which media types to affect (null = all)

RevertPolicy:
  mode: string                  # "revert" (restore previous intent),
                                # "hold" (keep new intent until manually changed),
                                # "timeout" (revert after duration)
  timeout_ms: uint?             # for "timeout" mode
```

**Condition sources** — what can trigger an intent binding:

| Source | Fields | Example |
|--------|--------|---------|
| `activity` | `state`, `title`, `app_id`, `platform` | `activity.state eq gaming` |
| `device` | `type`, `id`, `health`, any property | `device.type eq gamepad` (gamepad connected) |
| `sensor` | `reading`, `value`, `type` | `sensor.type eq motion AND sensor.value eq true` |
| `time` | `hour`, `day`, `weekday` | `time.hour gt 22` (after 10pm) |
| `power` | `battery_percent`, `charging`, `source` | `power.battery_percent lt 20` |
| `link` | `latency_ms`, `jitter_ms`, `loss` | `link.latency_ms gt 100` (high-latency path) |
| `presence` | `user_zone`, `idle_seconds`, `active_input` | `presence.idle_seconds gt 300` |
| `calendar` | `event_active`, `event_title`, `attendees` | `calendar.event_active eq true` |
| `input` | `active_keyboard`, `active_mouse` | `input.active_keyboard eq wireless-kb-1` |

**Examples**:

```yaml
# Game detected → switch to gaming intent
- id: auto-gaming
  conditions:
    - { source: activity, field: state, op: eq, value: gaming }
  intent: gaming
  scope: { target: node, target_id: gaming-pc }
  revert: { mode: revert }

# User idle for 5 minutes → switch to observe (save resources)
- id: idle-observe
  conditions:
    - { source: presence, field: idle_seconds, op: gt, value: 300 }
  intent: observe
  scope: { target: all }
  revert: { mode: revert }
  priority: 10

# Wireless keyboard active → user is at couch, prefer TV output
- id: couch-mode
  conditions:
    - { source: input, field: active_keyboard, op: eq, value: wireless-kb-couch }
  intent: desktop
  scope: { target: all }
  revert: { mode: revert }

# Battery low on phone → reduce phone screen mirror quality
- id: phone-battery-saver
  conditions:
    - { source: power, field: battery_percent, op: lt, value: 20 }
    - { source: device, field: type, op: eq, value: phone }
  intent: preview
  scope: { target: device, streams: ["video"] }
  revert: { mode: revert }
```

---

## 9. Topology Discovery

Topology discovery builds the graph from what the OS and devices report. It runs
continuously — initial discovery on startup, then hotplug-driven updates.

### 9.1 Discovery layers

Discovery happens in layers, from physical to logical:

**Layer 1: Hardware enumeration**

Each device plugin discovers its device class:

- USB: enumerate controllers, hubs, devices, their tree structure and speeds
- Thunderbolt: enumerate dock topology, internal USB hubs, display outputs
- PCI: enumerate capture cards, GPUs, network cards
- Network: enumerate interfaces, link speeds, routing tables

This produces devices and ports with `reported` quality properties.

**Layer 2: OS interface mapping**

Map hardware to OS-level interfaces:

- USB capture card → V4L2 device node → resolution/framerate capabilities
- USB audio device → ALSA device → PipeWire node → sample rate/channel capabilities
- USB HID device → `/dev/hidgN` → report descriptor → HID format
- Network interface → IP address → reachability to other Ozma nodes

This enriches ports with capability information at `reported` quality.

**Layer 3: Network topology**

Discover other Ozma devices on the network:

- mDNS discovery (existing spec 01) → node inventory
- Direct registration (for nodes behind NAT/SLIRP)
- Connect relay topology (for remote nodes)

This creates cross-machine links with `reported` quality for network properties.

**Layer 4: Active measurement**

Probe links to get `measured` quality data:

- Bandwidth: send probe packets, measure throughput
- Latency: RTT measurement (subtract processing time)
- Jitter: statistical analysis of packet timing
- Loss: count sent vs received over a window

Active measurement runs periodically on standby links and continuously (passive
observation) on active links.

**Layer 5: Capability enrichment**

Combine information from multiple sources:

- USB capture card reports 1080p60 capability (V4L2 enumeration, `reported`)
- USB bus is USB 2.0 (sysfs, `reported`) — bandwidth limited to ~280 Mbps
- Measured throughput is 240 Mbps (`measured`)
- Therefore: effective capability is 1080p30 MJPEG, not 1080p60 raw
  (derived from combining `reported` + `measured` data)

This is where information quality becomes critical. The capture card *says* it
can do 1080p60, but the USB bus can't carry it uncompressed. The router must
combine multiple quality-tagged properties to compute effective capabilities.

### 9.2 Opaque devices

Some devices are partially or fully opaque — their internal topology cannot be
discovered. Thunderbolt docks are the canonical example: the dock may use USB 2.0
internally for its hub while advertising USB 3.0 on its external ports.

**Strategy for opaque devices**:

1. Report what the OS tells us (`reported` quality)
2. Apply `assumed` defaults for unknown internals (e.g., assume dock USB hub
   matches external port speed)
3. Measure actual throughput to override assumptions (`measured`)
4. Allow user override (`user` quality) for anything the system gets wrong

Over time, the device database can accumulate known internal topologies for
specific dock models (by VID/PID), upgrading `assumed` to `spec`.

### 9.3 Compound device decomposition

Compound devices (docks, KVM cables, USB hubs with integrated audio) are
decomposed into sub-devices with internal links:

```
Thunderbolt Dock (VID:PID 0x1234:0x5678)
├── USB Hub (internal)
│   ├── Port: usb-downstream-1 (USB 3.0, reported)
│   ├── Port: usb-downstream-2 (USB 3.0, reported)
│   └── Port: usb-upstream (Thunderbolt, reported)
│       └── Internal Link → Thunderbolt Controller
├── Ethernet Adapter (internal)
│   ├── Port: ethernet (1 Gbps, reported)
│   └── Port: usb-upstream (USB 3.0, reported)
│       └── Internal Link → USB Hub
├── DisplayPort MST Hub (internal)
│   ├── Port: dp-out-1
│   ├── Port: dp-out-2
│   └── Port: thunderbolt-upstream
│       └── Internal Link → Thunderbolt Controller
└── Thunderbolt Controller
    └── Port: thunderbolt-upstream (40 Gbps, reported)
        └── External Link → Host Thunderbolt Port
```

The router sees all of these as part of the graph. If a capture card is
connected to `usb-downstream-1`, the router traces the path: capture card →
USB hub → Thunderbolt controller → host → and knows the bottleneck is the
USB hub's internal bandwidth, not the Thunderbolt link.

### 9.4 Topology calibration

When a device's internal topology is unknown or only partially known (the
dock example above — the OS reports "USB 3.0" on all downstream ports, but
some may actually be USB 2.0 internally), the system can run a **calibration
probe** to replace `assumed` or `spec` data with `measured` data.

**Calibration probe protocol**:

```yaml
CalibrationProbe:
  target: DeviceRef             # device to calibrate
  probe_type: string            # type of probe to run
  state: string                 # "pending", "running", "complete", "failed"
  results: CalibrationResult[]  # per-link/port results
  triggered_by: string          # "auto_discovery", "user_request", "schedule"
  disruptive: bool              # does this probe disrupt active pipelines?

CalibrationResult:
  entity: string                # port or internal link ID
  property: string              # what was measured ("bandwidth_bps", "latency_ms", "usb_speed")
  before: QualifiedValue        # previous value + quality
  after: QualifiedValue         # measured value + quality
  contributed_to_db: bool       # was this result submitted to the device database?
```

**Probe types**:

| Probe type | What it measures | Disruptive? | How |
|-----------|-----------------|-------------|-----|
| `usb_bandwidth` | Actual throughput per USB port | Yes — sends bulk data | Loopback transfer test via gadget or known device |
| `network_bandwidth` | Link throughput | Minimally — background traffic | iperf-style probe between endpoints |
| `latency` | Per-hop round-trip time | No | Timing probes on existing or probe traffic |
| `usb_speed_class` | Actual USB speed (not just reported) | No | Read from sysfs after device connection |
| `power_delivery` | Actual available current per port | Yes — draws increasing load | Controlled current draw test (requires INA219 or PD) |
| `display_edid` | Display capabilities | No | Read EDID from connected display |
| `audio_loopback` | Audio path latency and quality | Yes — sends test tone | Loopback measurement through audio pipeline |

**Auto-calibration triggers**:

- New unknown device detected (no device database match) → probe USB speed
  class and bandwidth
- Device database entry has `confidence: "assumed"` or `"estimated"` for
  internal topology → offer calibration
- User plugs a capture card into a new port → probe that port's actual
  bandwidth

**Results feed back into the device database** (§15). When a user calibrates
a specific dock model and discovers its internal USB hub is actually USB 2.0
on ports 3–4, that result can be submitted to Connect. Future users with the
same dock (matched by VID/PID) get the corrected topology automatically —
no calibration needed.

**API**:

```
POST /api/v1/routing/calibrate/{device_id}          # start calibration
GET  /api/v1/routing/calibrate/{device_id}/status    # probe status
GET  /api/v1/routing/calibrate/{device_id}/results   # probe results
POST /api/v1/routing/calibrate/{device_id}/submit    # submit results to device database
```

---

## 10. Security

### 10.1 Data plane encryption

The routing protocol distinguishes between control plane and data plane
encryption:

**Control plane**: All control messages (topology exchange, capability
advertisement, route negotiation, health metrics) travel over TLS or within
the WireGuard mesh. This is not changed by this specification.

**Data plane**: Media streams (video, audio, HID) use transport-level encryption
appropriate to the path:

| Path | Default encryption | Rationale |
|------|-------------------|-----------|
| Same machine (loopback) | None | Kernel boundary is sufficient |
| LAN (same broadcast domain) | XChaCha20-Poly1305 AEAD per packet | Lightweight, no tunnel overhead, no TCP overhead |
| WireGuard mesh (overlay) | WireGuard (ChaCha20-Poly1305) | Already encrypted by tunnel |
| Connect relay (remote) | WireGuard end-to-end | Relay sees only ciphertext |

**LAN encryption detail**: For UDP data plane traffic on the LAN (not tunnelled
through WireGuard), each packet is encrypted with XChaCha20-Poly1305 using a
session key established via a Noise NK handshake at link setup time. This
provides:

- Per-packet authentication and encryption
- No TCP overhead (critical for real-time media)
- No tunnel overhead (WireGuard adds ~60 bytes/packet)
- Forward secrecy (session keys are ephemeral)

The intent's `encryption` constraint controls this:
- `required`: every link must encrypt (default for all intents)
- `preferred`: encrypt if the transport supports it, allow unencrypted otherwise
- `none`: explicitly disable encryption (for debugging or trusted networks)

### 10.2 Authentication

Devices authenticate during discovery/enrollment using the existing mesh CA
(Ed25519 identity keys). The routing protocol does not introduce new
authentication — it relies on the identity layer established by the security
architecture (see `security.md`).

A device's identity is verified before its ports and capabilities are added to
the graph. Unauthenticated devices are never routed to.

---

## 11. Observability and Monitoring

The routing graph is a **monitoring platform by construction**. The data the
router needs to make routing decisions — device resources, link health, power
state, versions, topology — is exactly the data a monitoring platform needs.
Exposing it is a read path on data that already exists, not a separate system.

This is a deliberate architectural property: any system that joins the Ozma
mesh is automatically monitored. There is no separate monitoring agent to
install, no secondary data collection pipeline, no configuration. The node
needs to report its CPU usage so the router can avoid overloading it — that
same data stream is your CPU monitoring. The power model needs to track
voltage rails so the router can avoid brownouts — that same data is your
power monitoring. The device database needs to know firmware versions so the
router can check protocol compatibility — that same data is your asset
inventory.

**What you get for free by being in the mesh**:

| Monitoring domain | Source in the routing spec | Traditional tool replaced |
|------------------|--------------------------|---------------------------|
| Resource utilisation (CPU, memory, GPU, disk) | §2.7 Device Capacity | Prometheus node_exporter, Datadog agent |
| Network health (latency, jitter, loss, bandwidth) | §2.3 Link properties | SmokePing, Nagios, PRTG |
| Power (voltage, current, rail health, battery) | §2.10 Power Model | Custom INA219 scripts, UPS monitoring |
| Thermal | §2.7 ResourceType | lm-sensors polling |
| Asset inventory (hardware, versions, topology) | §15 Device Database + §14 Versioning | Snipe-IT, GLPI, Lansweeper |
| Topology mapping | §9 Topology Discovery | nmap, network mapping tools |
| Service health | §2.9 Managed Services | Uptime Kuma, Healthchecks.io |
| USB device tree | §9.1 Layer 1 Hardware Enumeration | lsusb scripts |
| Storage health | §15 StorageSpec | smartctl polling |
| Event stream (state changes, alerts, threshold crossings) | §11.3 Events | Alertmanager, PagerDuty integration |

### 11.1 Graph queries

```
GET /api/v1/routing/graph          # full graph (devices, ports, links)
GET /api/v1/routing/graph/devices  # all devices with ports
GET /api/v1/routing/graph/links    # all links with metrics
GET /api/v1/routing/pipelines      # all active and standby pipelines
GET /api/v1/routing/pipelines/{id} # pipeline detail with per-hop metrics
GET /api/v1/routing/intents        # all defined intents (built-in + custom)
GET /api/v1/routing/intents/{name} # intent definition
GET /api/v1/routing/devices/{id}/capacity  # device resource capacity and current usage
GET /api/v1/routing/devices/{id}/budget    # device resource budget and compliance
GET /api/v1/routing/pressure               # all devices with resource pressure summary
PUT /api/v1/routing/devices/{id}/budget    # set/update resource budget for a device
```

### 11.2 Monitoring queries

Queries designed for monitoring dashboards and alerting, not just routing
diagnostics:

```
# Time-series data for any device metric
GET /api/v1/monitoring/metrics/{device_id}?keys=cpu_percent,memory_mb&range=1h
# Returns: timestamped metric values over the requested window.

# Fleet-wide health summary
GET /api/v1/monitoring/health
# Returns: per-device health status (healthy, degraded, critical, unreachable),
# aggregated by device type, with worst-offender highlighting.

# All current alerts/warnings across the fleet
GET /api/v1/monitoring/alerts
# Returns: active threshold crossings (power, thermal, resource, network).

# Historical link quality between two devices
GET /api/v1/monitoring/link/{link_id}/history?range=24h
# Returns: latency/jitter/loss/bandwidth time series.

# Power dashboard — all rails across all devices
GET /api/v1/monitoring/power
# Returns: per-rail voltage, current, capacity, health status.

# Asset inventory snapshot
GET /api/v1/monitoring/inventory
# Returns: all devices with hardware info, versions, location, health.
# Exportable as CSV/JSON for compliance evidence.
```

### 11.3 Diagnostic queries

```
# "Why did the router choose this pipeline?"
GET /api/v1/routing/explain?source={port}&dest={port}&intent={name}
# Returns: all candidate pipelines, their costs, which constraints eliminated
# each rejected candidate, and why the selected pipeline won.

# "What would happen if this link failed?"
GET /api/v1/routing/simulate?fail_link={link_id}
# Returns: which pipelines would be affected, what alternatives exist,
# expected degradation.

# "Can this intent be satisfied?"
POST /api/v1/routing/evaluate
# Body: { source, destination, intent }
# Returns: best pipeline if possible, or the closest match with which
# constraints are violated and what degradation would be needed.

# "What's consuming power on this device?"
GET /api/v1/routing/explain/power/{device_id}
# Returns: per-rail breakdown, per-function power attribution,
# headroom analysis, warnings.
```

### 11.4 Metric retention and export

The routing graph is real-time — it reflects current state. For historical
data, metrics are retained locally with configurable windows:

```yaml
MetricRetention:
  high_resolution: duration     # 1-second samples (default: 1 hour)
  medium_resolution: duration   # 1-minute aggregates (default: 24 hours)
  low_resolution: duration      # 15-minute aggregates (default: 30 days)
  export_targets: MetricsSink[] # external systems to push to (§2.9)
```

Export to external systems uses the `metrics_sink` device type (§2.9).
Supported formats: Prometheus exposition (scrape endpoint), OTLP push,
StatsD, syslog. The controller acts as the collection point — nodes report
to the controller, the controller exports to external sinks.

For users who don't want external monitoring infrastructure, the built-in
retention is sufficient. For users who already have Prometheus/Grafana or
Datadog, the export path feeds into their existing stack — Ozma data appears
alongside everything else they monitor.

### 11.5 Events

State changes emit events on the WebSocket event stream. These serve both
routing (the router reacts to them) and monitoring (dashboards and alerting
consume them):

```
# Pipeline lifecycle
routing.pipeline.created    # new pipeline computed
routing.pipeline.warming    # pipeline transitioning from standby to warm
routing.pipeline.warm       # pipeline is warm and ready for fast activation
routing.pipeline.activating # pipeline transitioning from warm/standby to active
routing.pipeline.activated  # pipeline is active — data flowing
routing.pipeline.cooling    # pipeline transitioning from warm to standby (resource release)
routing.pipeline.degraded   # pipeline quality dropped below intent preferences
routing.pipeline.failed     # pipeline link failed, failover in progress
routing.pipeline.recovered  # failed pipeline restored or replaced

# Device lifecycle
routing.device.discovered   # new device added to graph
routing.device.removed      # device removed from graph
routing.device.pressure     # device resource pressure changed (approaching/exceeding limits)
routing.device.budget_breach # device exceeded resource budget backoff threshold

# Link and network
routing.link.measured       # link metrics updated
routing.link.degraded       # link quality dropped below threshold
routing.link.recovered      # link quality restored

# Power (§2.10)
routing.power.rail_warning       # voltage or current approaching limits
routing.power.rail_critical      # voltage below min or current exceeding capacity
routing.power.budget_exceeded    # device power draw exceeds source capacity
routing.power.pd_negotiated      # USB PD negotiation completed
routing.power.battery_low        # battery below threshold
routing.power.rgb_power_limited  # RGB brightness scaled down due to power limit

# Versioning (§14)
device.version.update_available  # new version detected
device.version.updating          # applying update
device.version.updated           # update succeeded
device.version.update_failed     # update failed
device.version.incompatible      # version mismatch

# Intent and routing
routing.intent.changed      # intent definition updated

# Thermal
device.thermal.warning       # approaching thermal throttle
device.thermal.throttling    # actively thermal throttling
device.thermal.recovered     # temperature returned to normal

# Trend / predictive (§11.7)
trend.degradation_detected   # metric trending toward failure (e.g., increasing USB error rate)
trend.capacity_warning        # resource approaching exhaustion at current rate (storage, battery wear)
trend.lifetime_estimate       # estimated time until failure/exhaustion, based on trend
trend.anomaly_detected        # metric deviated significantly from historical baseline
```

Every event includes a timestamp, the device/link/pipeline ID, severity
(`info`, `warning`, `critical`), and a structured payload. Events can be
forwarded to notification sinks (§2.9) based on configurable rules —
"send Slack message on any `critical` event", "email on `device.version.update_failed`".

### 11.6 State change journal

Every state change in the routing graph — device discovered/removed, link
up/down, port connected/disconnected, power rail change, version change,
configuration change — is optionally recorded to a persistent journal. This
is distinct from the real-time event stream (§11.5): events are ephemeral
(consumed by listeners), the journal is durable (queryable history).

**What constitutes a state change**:

Any mutation to the routing graph is a state change. The journal captures
the graph diff — what changed, from what to what, when, and why:

```yaml
StateChangeRecord:
  id: uint64                    # monotonically increasing sequence number
  timestamp: timestamp          # when the change occurred
  change_type: StateChangeType  # what kind of change
  entity_type: string           # "device", "port", "link", "pipeline", "rail", "config"
  entity_id: string             # which entity changed
  device_id: string?            # device the change relates to (if applicable)
  before: any?                  # previous state (null for creation)
  after: any?                   # new state (null for deletion)
  trigger: string?              # what caused the change ("hotplug", "user", "router",
                                # "measurement", "timeout", "enrollment", "ota")
  metadata: PropertyBag?        # additional context

StateChangeType: enum
  # Device lifecycle
  device_added                  # new device appeared in graph (USB hotplug, mDNS, enrollment)
  device_removed                # device left the graph (unplug, timeout, revocation)
  device_state_changed          # device property changed (health, capacity, version)

  # Port lifecycle
  port_connected                # something plugged into this port
  port_disconnected             # something unplugged from this port
  port_state_changed            # port properties changed (format, bandwidth, active state)

  # Link lifecycle
  link_created                  # new link between ports
  link_removed                  # link broken
  link_state_changed            # link metrics changed significantly (latency, jitter, loss)

  # Pipeline lifecycle
  pipeline_created
  pipeline_activated
  pipeline_warmed
  pipeline_cooled
  pipeline_degraded
  pipeline_failed
  pipeline_recovered
  pipeline_destroyed

  # Power
  rail_voltage_changed          # voltage moved outside normal band
  rail_current_changed          # significant current change
  power_source_changed          # device switched power source (e.g., battery ↔ mains)
  pd_negotiation_changed        # USB PD renegotiated

  # Configuration
  config_changed                # user changed a setting
  intent_changed                # intent definition updated
  budget_changed                # resource budget modified
  gadget_function_changed       # USB gadget function added/removed/modified

  # Identity
  device_enrolled               # device joined mesh
  device_revoked                # device removed from mesh
  certificate_issued            # new certificate from mesh CA
  certificate_expired           # certificate expired
```

**Storage policy**:

```yaml
JournalPolicy:
  enabled: bool                 # default: true
  storage: string               # "sqlite", "append_log", "memory_ring"
  retention: JournalRetention
  filters: JournalFilter[]      # which changes to record (default: all)

JournalRetention:
  max_records: uint?            # maximum records to keep (oldest evicted)
  max_age_days: uint?           # delete records older than this
  max_size_mb: uint?            # maximum storage size

JournalFilter:
  include: StateChangeType[]?   # only record these types (null = all)
  exclude: StateChangeType[]?   # never record these types
  min_severity: string?         # only record changes at or above this severity
  devices: string[]?            # only record changes for these devices (null = all)
```

Default policy: record everything, keep 30 days, cap at 100 MB. For
resource-constrained nodes, the policy can be narrowed — e.g., only record
device adds/removes and power events, keep 7 days.

**Querying the journal**:

```
GET /api/v1/monitoring/journal?range=24h
GET /api/v1/monitoring/journal?device_id={id}&range=7d
GET /api/v1/monitoring/journal?change_type=device_added,device_removed&range=30d
GET /api/v1/monitoring/journal?entity_type=link&range=1h
GET /api/v1/monitoring/journal/{sequence_id}  # single record with full before/after
```

**Use cases**:

- **"When was this USB device last plugged in?"** — query `port_connected`
  for the device's USB port
- **"What changed in the last hour?"** — query all changes, range=1h
- **"Why did the RGB strip go dark at 3am?"** — query power events and
  device state changes for that time window
- **"Show me every firmware update across the fleet this month"** — query
  `device_state_changed` where the version field differs
- **Compliance audit trail** — the journal provides a tamper-evident record
  of every graph mutation (ties into the existing hashchained audit log)
- **Debugging intermittent issues** — USB device dropping and reconnecting
  shows up as repeated `port_disconnected` / `port_connected` pairs with
  timestamps, revealing the pattern

**Relationship to the audit log** (`controller/audit_log.py`): The audit log
is a hashchained compliance record of security-relevant actions (authentication,
authorization, configuration changes). The state change journal is broader —
it records all graph mutations including non-security events (USB hotplug,
link metric changes, power fluctuations). The audit log is a strict subset
of the journal with cryptographic integrity guarantees. Both can be enabled
independently.

### 11.7 Trend analysis

The journal (§11.6) and metric retention (§11.4) store historical data. Trend
analysis detects patterns in that data — degradation over time, capacity
approaching exhaustion, anomalous behaviour — and emits predictive events
before problems occur.

The spec defines the **output model** (what trend alerts look like), not the
algorithms (that's implementation).

**Trend alert structure**:

```yaml
TrendAlert:
  id: string
  type: TrendAlertType
  device_id: string             # affected device
  metric: string                # which metric is trending ("usb_error_rate",
                                # "link_jitter_p95", "storage_used_percent",
                                # "battery_health_percent", "rail_voltage_5v")
  current_value: float          # current value of the metric
  trend_direction: string       # "increasing", "decreasing", "unstable"
  rate_of_change: float         # change per hour (in metric's native unit)
  projected_threshold_value: float?  # when will it cross the threshold?
  projected_time: timestamp?    # estimated time of threshold crossing
  confidence: float             # 0.0–1.0 confidence in the projection
  window: string                # time window the trend was computed over ("24h", "7d", "30d")
  severity: string              # "info", "warning", "critical"
  recommendation: string?       # human-readable suggestion ("Replace USB cable",
                                # "Upgrade power supply", "Expand storage")

TrendAlertType: enum
  degradation                   # metric trending toward failure
  capacity_exhaustion           # resource approaching full (storage, battery, power)
  lifetime_estimate             # projected device/component lifetime
  anomaly                       # metric deviated from historical baseline
  recurring_failure             # same failure happening repeatedly (e.g., USB disconnect/reconnect)
```

**What can be trended**:

| Metric | Trend indicates | Example alert |
|--------|----------------|---------------|
| USB error rate | Cable/connector degradation | "USB error rate on Node 1 port 2 increasing 3%/week — cable may be failing" |
| Link jitter (p95) | Network path degradation | "WiFi jitter to Node 3 increased 40% over 7 days — possible interference" |
| Storage used % | Capacity exhaustion | "Recording storage on Node 2 will be full in ~12 days at current rate" |
| Battery health % | Battery wear | "Phone battery at 78% health, losing ~2%/month" |
| Rail voltage | Power supply degradation | "5V rail on Node 1 averaging 4.81V, down from 4.92V 30 days ago" |
| SSD write latency | Drive wear | "NVMe write latency up 25% over 90 days — approaching endurance limit" |
| Thermal (idle temp) | Cooling degradation | "Idle temp on Node 4 trending up 0.5°C/week — check airflow/thermal paste" |
| Reconnection frequency | Intermittent hardware fault | "Capture card on Node 1 has disconnected 4 times this week, up from 0 last month" |

**API**:

```
GET /api/v1/monitoring/trends                         # all active trend alerts
GET /api/v1/monitoring/trends/{device_id}             # trends for a specific device
GET /api/v1/monitoring/trends/{device_id}/{metric}    # trend detail with historical data points
```

---

## 12. Relationship to Existing Protocols

This routing specification is a **layer above** the existing protocol specs. It
does not replace them — it provides the model that determines when and how they
are used.

| Existing spec | Relationship to routing |
|--------------|------------------------|
| 01 — Discovery (mDNS) | Discovery layer 3: populates graph with network-visible nodes |
| 02 — HID Transport | Transport plugin: `udp-direct` or `udp-aead` carrying HID format |
| 03 — Audio: VBAN | Transport plugin: `vban` carrying uncompressed audio format |
| 04 — Audio: Opus RTP | Transport plugin: `rtp-opus` carrying compressed audio format |
| 05 — Video: MJPEG/UVC | Transport plugin: `udp-mjpeg` carrying MJPEG video format |
| 06 — Video: H.265 Sunshine | Transport plugin: `sunshine` carrying H.265 video format |
| 07 — Control Plane | The API through which routing is observed and controlled |
| 08 — OTA | Device versioning and update delivery (now §14) |
| 09 — Event/Command | Control plane transport for node commands including pipeline activation |
| 10 — Presence/Display | Device plugin: presence nodes as sensor/data sources; display nodes as screen sinks |
| 11 — Peripheral RGB | Transport + format: `rgb` media type with DDP/Art-Net/E1.31/vendor transports |

**Additional protocols not in the spec series but modelled in the routing graph**:

| Protocol/System | Relationship to routing |
|----------------|------------------------|
| WebRTC | Transport plugin: browser-based video/audio/HID with DTLS |
| Looking Glass (IVSHMEM) | Transport plugin: zero-copy VM display via shared memory |
| QMP / libvirt | Device plugin: VM host discovery, HID injection, power control |
| RTSP / ONVIF / NDI | Device + transport plugins: camera discovery, video source, PTZ control |
| Bluetooth (A2DP/HFP/BLE) | Transport plugin: audio and control over Bluetooth |
| MQTT | Transport plugin: IoT sensor/actuator data, doorbell events |
| KDE Connect | Transport + device plugin: phone as compound device |
| OSC | Transport plugin: network control surface input/feedback |
| MIDI | Transport plugin: control surface with bidirectional feedback |
| DDC/CI | Transport plugin: monitor brightness, power, input switching |
| CEC | Transport plugin: HDMI device control, switch commands |
| Serial (RS-232) | Transport plugin: switch/actuator control, serial consoles |
| NUT | Transport + device plugin: UPS monitoring |
| WoL | Transport plugin: Wake-on-LAN magic packets to target devices |
| DDP / Art-Net / E1.31 | Transport plugins: network RGB protocols |
| PipeWire | Transport plugin: same-machine audio linking |

The routing protocol's job is to decide: for a given intent, which of these
protocols should be activated, with what parameters, between which endpoints.
The individual protocols remain the wire-level implementation.

---

## 13. Implementation Guidance

This section is non-normative. It describes how this specification is expected to
be implemented, without constraining the implementation.

### 13.1 Incremental adoption

The routing model can be adopted incrementally over the existing codebase:

1. **Phase 1: Graph model** — Build the graph data structures. Populate them from
   existing discovery (mDNS, V4L2 enumeration, PipeWire node listing). No
   routing changes — the graph is observational only.

2. **Phase 2: Intent system** — Define intents. Map existing scenario switching to
   intent-driven pipeline selection. The router recommends pipelines; the
   existing code activates them.

3. **Phase 3: Format negotiation** — Add capability enumeration to ports. The
   router negotiates formats instead of hardcoding them.

4. **Phase 4: Transport plugins** — Factor existing transport code (UDP HID,
   VBAN, RTP) into the plugin interface. New transports can be added without
   modifying core routing.

5. **Phase 5: Active measurement** — Add link probing. Replace `assumed` and
   `spec` quality data with `measured` data. Enable dynamic re-evaluation.

6. **Phase 6: Full routing** — The router assembles and manages pipelines
   end-to-end. The existing protocol-specific code becomes transport plugins.

### 13.2 Performance considerations

- Graph operations (path finding, format negotiation) must complete in <1ms for
  typical graphs (<100 devices, <500 links)
- Pipeline switching (activating a pre-computed pipeline) must complete in <10ms
  for local KVM, <100ms for remote access
- Measurement probing must not interfere with active pipelines (use separate
  low-priority traffic)
- The graph should be stored in memory, not persisted — it is rebuilt from
  discovery on each startup

### 13.3 PipeWire integration

On machines running PipeWire, the Ozma routing graph integrates with
PipeWire's graph rather than duplicating it. The two systems have parallel
models — every Ozma audio concept maps to a PipeWire primitive:

**Node mapping**:

| Ozma concept | PipeWire equivalent | Implementation |
|-------------|-------------------|----------------|
| Audio source device | PipeWire node (Audio/Source) | Discovered via `pw-dump` |
| Audio sink device | PipeWire node (Audio/Sink) | Discovered via `pw-dump` |
| Mix bus (§2.13) | PipeWire node with N input port groups | `pw-filter-chain` summing node or `module-null-sink` + volume controls |
| Monitor controller (§2.13) | Combination of `pw-link` + volume + filter-chain | Source selection = link management, dim/mono = filter-chain |
| Insert chain processor (§2.13) | `pw-filter-chain` node | One filter-chain per insert slot, linked in series |
| Room correction EQ | `pw-filter-chain` with biquad filters | `ozma-room-eq` capture → EQ bands → playback |
| VBAN network bridge | `pw-cat` virtual source/sink | VBAN receiver → `pw-cat --playback`; `pw-cat --capture` → VBAN sender |
| Audio output target | PipeWire module sink | `module-raop-sink`, `module-rtp-sink`, `module-roc-sink`, etc. |
| Delay compensation (§2.13) | `pw-loopback --delay` | Per-output delay alignment |
| Cue send (§2.13) | Mix bus node with independent volume controls | Separate PipeWire node per cue mix |

**Port mapping**:

| Ozma concept | PipeWire equivalent |
|-------------|-------------------|
| Audio port (source) | PipeWire output port(s) — one per channel (FL, FR, etc.) |
| Audio port (sink) | PipeWire input port(s) — one per channel |
| Channel map (§2.13) | PipeWire port `audio.channel` property |
| Port power budget | N/A in PipeWire (Ozma-only concept) |

**Link mapping**:

| Ozma concept | PipeWire equivalent |
|-------------|-------------------|
| Audio link (same machine) | PipeWire Link object between ports |
| Audio link (cross-machine) | VBAN/Opus transport → `pw-cat` bridge → PipeWire link |
| Link metrics | PipeWire node `Props` (volume, mute, latency) |
| Format negotiation | PipeWire SPA format enumeration (same-machine); Ozma format negotiation (cross-machine) |

**Routing modes**:

1. **pw-link mode** (default): Ozma calls `pw-link` directly to connect
   PipeWire nodes. Simple, works for basic KVM audio. Node-level granularity
   (all channels auto-mapped).

2. **WirePlumber mode** (`OZMA_AUDIO_WIREPLUMBER=1`): Ozma writes a single
   metadata key (`pw-metadata -n ozma set 0 active_node <name>`).
   WirePlumber's Lua script (`ozma-routing.lua`) watches this and manages
   port-level links with explicit channel mapping. More reliable for
   multichannel audio and complex scenarios.

3. **Router mode** (Phase 3+): The routing graph computes audio pipelines
   and translates them to PipeWire operations. WirePlumber executes the
   link commands. The router handles format negotiation, insert chain
   assembly, mix bus creation, and latency compensation. WirePlumber is
   the executor, not the decision-maker.

**Clock mapping**:

| Ozma concept | PipeWire equivalent |
|-------------|-------------------|
| Clock domain (§7) | PipeWire driver node |
| Clock master | Driver node's hardware clock |
| Sample-accurate sync (§2.13) | PipeWire clock class + rate matching |
| Drift compensation | PipeWire adaptive resampling (built-in) or Ozma-managed `pw-loopback` |

PipeWire already handles same-machine clock synchronisation via its
driver/follower model. Ozma's clock model (§7) extends this across machines
— PTP/NTP provide inter-machine sync, and PipeWire handles intra-machine
scheduling. The two complement each other.

**Metering mapping**:

PipeWire nodes expose peak levels via the `Props` parameter
(`channelVolumes`, `softVolumes`). For pro audio metering (§2.13 — LUFS,
true peak, VU), a dedicated `pw-filter-chain` analysis node is inserted at
each metering point. The analysis node reads audio data and computes metrics
without modifying the signal (wet_dry = 0.0).

**What PipeWire handles natively (Ozma should not duplicate)**:

- Same-machine buffer management and zero-copy transport
- Hardware device enumeration and driver management
- Format negotiation for same-machine links (SPA format)
- Driver/follower clock scheduling within a machine
- Adaptive resampling for same-machine clock domain mismatches
- Port-level channel routing

**What Ozma adds on top of PipeWire**:

- Cross-machine audio routing (VBAN, Opus, AES67)
- Intent-driven pipeline selection (which sources go where)
- Mix bus and monitor controller as managed virtual devices
- Insert chain orchestration (processor ordering and bypass)
- Cross-machine clock sync (PTP/NTP)
- Power-aware routing (audio device power budgets)
- Monitoring, journaling, and trend analysis on audio paths
- Device database integration (microphone response curves, speaker specs)

---

## 14. Device Versioning and Mesh Updates

Every Ozma-managed device in the graph — controllers, hardware nodes, soft
nodes, desktop agents, screen firmware, RGB controllers — has a software or
firmware version. The routing protocol tracks versions as part of the graph and
provides a mechanism for updating devices through the mesh.

### 14.1 Version model

```yaml
DeviceVersion:
  component: string             # "controller", "node", "agent", "softnode",
                                # "screen_firmware", "rgb_firmware", "plugin"
  current_version: SemVer       # currently running version
  channel: string               # "stable", "beta", "nightly", "pinned"
  platform: string              # "linux-amd64", "linux-arm64", "linux-riscv64",
                                # "windows-amd64", "macos-arm64", "esp32", "rp2040"
  build_info: BuildInfo?        # build metadata
  update_state: UpdateState     # current update status
  protocol_version: string      # ozma protocol version this device speaks ("ozma/0.1")
  min_compatible: string?       # minimum controller version this device works with
  max_compatible: string?       # maximum controller version (if known)

BuildInfo:
  commit: string?               # git commit hash
  build_date: timestamp?        # when this build was produced
  edition: string?              # "open_source", "free", "commercial"
  signature: string?            # Ed25519 signature (base64)
  signed_by: string?            # signing key identifier

UpdateState:
  status: up_to_date | update_available | updating | update_failed | unknown
  available_version: SemVer?    # newest version available (null if up_to_date or unknown)
  available_channel: string?    # channel of available update
  last_check: timestamp?        # when we last checked for updates
  last_update: timestamp?       # when the device was last updated
  failure_reason: string?       # if update_failed, why
  can_update: bool              # does this device support remote update?
  requires_reboot: bool?        # will the update require a restart?
  rollback_available: bool?     # can this device roll back to previous version?
```

### 14.2 Version in the routing graph

Versions are a property of devices, not links. They affect routing in
several ways:

**Protocol compatibility**: A node running `ozma/0.1` may not support features
added in `ozma/0.2`. The router checks `protocol_version` and
`min_compatible`/`max_compatible` when assembling pipelines. If a node can't
speak the required protocol version for a transport or format, the pipeline is
rejected or a compatible fallback is selected.

**Feature capabilities**: Newer versions may support additional formats, codecs,
or transport types. The router discovers these via capability enumeration
(§4.2) — version is informational, capabilities are authoritative.

**Fleet consistency**: When devices in the mesh are at different versions, the
controller tracks this as a health indicator. Mixed-version deployments are
supported (backwards compatibility is mandatory within a major version), but
the controller surfaces version drift as a warning.

### 14.3 Update delivery through the mesh

Updates are delivered through the existing mesh infrastructure. The controller
acts as the update coordinator — it knows what versions are available, what each
device is running, and orchestrates the update process.

**Update sources**:

```yaml
UpdateSource:
  type: string                  # "connect", "local", "url", "manual"
  url: string?                  # for "connect": Connect API; for "url": direct HTTP
  check_interval_s: uint        # how often to check (default: 3600 = hourly)
  auto_update: bool             # automatically apply updates without user confirmation
  auto_update_window: TimeWindow? # only auto-update during this window (e.g., 02:00–05:00)
```

| Source | How it works | When |
|--------|-------------|------|
| `connect` | Controller checks Connect API for new versions | Default for registered controllers |
| `local` | Controller serves updates from its local filesystem | Air-gapped deployments |
| `url` | Controller fetches from a configured URL | Self-hosted update mirror |
| `manual` | User uploads update file via API | Emergency or testing |

**Update flow**:

```
1. Controller checks update source for new versions
2. For each device type with an available update:
   a. Check compatibility (protocol version, platform, dependencies)
   b. Download update artefact (if not already cached)
   c. Verify integrity (SHA-256) and authenticity (Ed25519 signature)
   d. If auto_update: proceed. Otherwise: notify user, wait for approval.
3. For each device to update:
   a. Check device health and resource state (don't update under pressure)
   b. For nodes: ensure no active pipeline depends solely on this node
      (wait for user to switch away, or failover to alternate path)
   c. Push update to device via the mesh (binary channel, spec 09, or HTTP)
   d. Device applies update (A/B partition, container restart, process restart)
   e. Device reports new version on reconnection
   f. Controller verifies capabilities and re-runs graph discovery
4. If update fails:
   a. Device rolls back (if supported)
   b. Controller marks device as update_failed with reason
   c. Alert user
```

**Update types by device class**:

| Device class | Update mechanism | Disruption | Rollback |
|-------------|-----------------|------------|----------|
| Controller | Process restart or container update | Brief API outage (<5s) | Previous container/binary |
| Hardware node (SBC) | A/B partition flash + reboot | Node offline during reboot (~30s) | Boot to previous partition |
| Soft node | Process restart | Node offline during restart (~2s) | Previous binary |
| Desktop agent | Process restart (background) | No disruption to user | Previous binary |
| Screen firmware (ESP32) | OTA flash via HTTP | Screen dark during flash (~30s) | Dual-partition if supported |
| RGB controller (WLED) | OTA flash via WLED API | LEDs off during flash (~15s) | WLED has its own rollback |
| Plugins | Hot-reload if possible, restart if not | Depends on plugin | Previous version |

### 14.4 Update orchestration

Updates across a fleet are orchestrated, not simultaneous:

**Rolling updates**: Nodes are updated one at a time (or in configurable batch
sizes). The controller waits for each node to come back healthy before
proceeding. If a node fails to return, the rollout is paused and the user is
alerted.

**Dependency ordering**: If an update requires a minimum controller version,
the controller updates itself first. If nodes require a minimum agent version
on their targets, agents are updated before nodes.

**Pipeline-aware scheduling**: The controller won't update a node that is
currently the active KVM target unless the user approves or there's a failover
path. For non-critical nodes (preview-only, monitoring), updates can proceed
automatically.

**Maintenance windows**: Auto-updates can be restricted to a time window
(`auto_update_window`). Outside the window, updates are downloaded and staged
but not applied until the window opens.

### 14.5 Version observability

```
GET /api/v1/devices/versions              # all devices with current version info
GET /api/v1/devices/{id}/version          # single device version detail
GET /api/v1/updates/available             # all available updates across the fleet
POST /api/v1/updates/check                # force an update check now
POST /api/v1/updates/apply                # apply pending updates (with options)
POST /api/v1/updates/apply/{device_id}    # update a specific device
GET /api/v1/updates/history               # update history (who, when, from→to, success/fail)
```

**Events**:

```
device.version.update_available  # new version detected for a device class
device.version.updating          # device is applying an update
device.version.updated           # device successfully updated (old→new version)
device.version.update_failed     # device update failed (includes reason)
device.version.rollback          # device rolled back to previous version
device.version.incompatible      # device version is incompatible with controller
```

### 14.6 Protocol version negotiation and the graph

When a device connects to the mesh, the first thing exchanged is protocol
version. This is already defined in spec 01 (mDNS `proto` TXT field) and
spec 09 (EVT_HELLO). The routing graph uses this to:

1. Set `protocol_version` on the device
2. Determine which format types and transport plugins the device supports
3. Filter capability enumeration to the intersection of controller and device
   protocol versions
4. Surface incompatibility warnings if the device is too old or too new

Within a major version, the controller speaks to the device at the device's
protocol level. A controller at `ozma/0.5` can route through a node at
`ozma/0.3` — it simply doesn't use features added in 0.4/0.5 on that path.
This is transparent to the router: the device's capability enumeration already
reflects what it can do, so the router never selects unsupported features.

---

## 15. Physical Device Database

The device database is a universal, open, community-contributed catalog of
physical device definitions. It provides the routing graph with detailed
knowledge about the real-world properties of hardware — dimensions, internal
topology, port capabilities, performance characteristics, frequency response
curves, 3D models, and anything else that helps Ozma be a better router.

### 15.1 Design principles

**Minimal required fields**: A device entry needs only an `id`, a `type`, and
a `name`. Everything else is optional. A bare-minimum entry gets a device into
the graph with `assumed` quality properties. As the community contributes more
detail, the entry gets richer and routing decisions get better.

**Maximum flexibility**: The schema is extensible by design. Any field not in
the core schema can be added as a typed extension. If one user discovers that
a specific Thunderbolt dock uses USB 2.0 internally, they add an
`internal_topology` block. That block becomes available to everyone with that
dock.

**Open and community-driven**: The database is public, version-controlled, and
accepts contributions. Anyone can submit a new device entry, correct an existing
one, or add detail. Contributions are reviewed and quality-tagged.

**Distributed via Connect**: The canonical database lives on Connect. Controllers
download the portions they need. Large controllers with storage cache
everything and distribute via mesh to nodes. Small nodes download only entries
matching their detected hardware.

### 15.2 Entry schema

Every device in the database follows this schema. Only `id`, `type`, and `name`
are required. Everything else is optional and typed.

```yaml
DeviceEntry:
  # --- Required ---
  id: string                    # globally unique slug (vendor-model-variant)
  type: string                  # device type (from §2.1 table, or any extension type)
  name: string                  # human-readable name ("Keychron Q1 Pro")

  # --- Identity ---
  vendor: string?               # manufacturer name
  model: string?                # model name/number
  variant: string?              # specific variant (color, revision, region)
  url: string?                  # product page URL
  usb: UsbIdentifier[]?         # USB VID/PID pairs for auto-matching
  bluetooth: BtIdentifier?      # Bluetooth device name pattern, OUI
  serial_pattern: string?       # serial port identification pattern
  network: NetworkIdentifier?   # mDNS type, MAC OUI, or other network ID

  # --- Physical ---
  dimensions_mm: Dimensions?    # { w, d, h } bounding box
  weight_g: float?              # weight in grams
  color: string?                # primary color/finish
  materials: string[]?          # ["aluminum", "abs_plastic", "tempered_glass"]

  # --- Inheritance ---
  inherits: string?             # parent entry ID (inherit all fields, override selectively)
  tags: string[]?               # searchable tags (e.g., ["mechanical", "hot-swap", "wireless"])

  # --- Category-specific blocks (all optional) ---
  motherboard: MotherboardSpec? # chipset, CPU socket, internal topology, physical port map
  cpu: CpuSpec?                 # CPU/SoC capabilities, cache, iGPU, memory controller
  chipset: ChipsetSpec?         # PCH/southbridge: what connects where, lane allocation
  keyboard: KeyboardSpec?
  mouse: MouseSpec?
  audio: AudioSpec?
  display: DisplaySpec?
  hub: HubSpec?
  dock: DockSpec?
  pcie: PcieCardSpec?           # PCIe cards (GPUs, NICs, capture cards, USB controllers, NVMe)
  capture: CaptureSpec?
  camera: CameraSpec?
  actuator: ActuatorSpec?
  sensor: SensorSpec?
  power: PowerSpec?
  network: NetworkCardSpec?     # NICs, WiFi cards, Bluetooth adapters
  storage: StorageSpec?         # SSDs, HDDs, NVMe drives
  gpu: GpuSpec?                 # GPUs (encode/decode capabilities, display outputs)
  rgb: RgbSpec?                 # LED layout, zones (see device-db.md for full spatial schema)
  screen: ScreenSpec?           # embedded screen capabilities
  control: ControlSpec?         # control surface capabilities
  furniture: FurnitureSpec?     # desks, racks, mounts — physical dimensions + slots + state
  network_switch: NetworkSwitchSpec?  # managed/unmanaged switches
  router: RouterSpec?           # routers, gateways, firewalls
  access_point: AccessPointSpec?  # WiFi access points

  # --- Topology ---
  internal_topology: InternalTopology?  # how ports connect inside this device

  # --- 3D ---
  shape_template: string?       # parametric template ID (see device-db.md)
  model_3d: Model3dRef?         # optional 3D model for rendering and recognition

  # --- Routing hints ---
  routing_hints: RoutingHints?  # performance characteristics for the router

  # --- Provenance ---
  sources: Source[]?            # where this data came from
  confidence: string?           # "exact", "approximate", "estimated", "community"
  contributors: string[]?       # who contributed this entry
  last_updated: string?         # ISO date of last update
```

### 15.3 Inheritance

Entries can inherit from a parent using `inherits`. This avoids duplicating
common data:

```yaml
# Base entry: generic 87-key TKL keyboard
id: template-tkl-87key
type: keyboard
name: Generic TKL 87-key
keyboard:
  layout: tkl
  key_count: 87
  standard: ansi
dimensions_mm: { w: 360, d: 135, h: 38 }

# Specific product inherits and overrides
id: keychron-q3-pro
type: keyboard
name: Keychron Q3 Pro
inherits: template-tkl-87key     # gets layout, key_count, standard
vendor: Keychron
model: Q3 Pro
dimensions_mm: { w: 365, d: 145, h: 40 }  # override with exact dimensions
usb: [{ vid: "0x3434", pid: "0x0930" }]
keyboard:
  layout: tkl                    # inherited, but explicit is fine
  key_count: 87                  # inherited
  switch_type: hot-swap          # added
  wireless: [bluetooth, usb_2_4ghz]  # added
  via_compatible: true           # added
  battery_mah: 4000              # added
rgb:
  led_count: 87
  zones: [{ id: 0, name: "all", leds: "0-86" }]
  # per-key positions imported from VIA layout
model_3d: { format: "glb", url: "connect://device-db/models/keychron-q3-pro.glb" }
```

Standard base types are provided for common form factors:

| Base entry | Description |
|-----------|-------------|
| `template-keyboard-full` | Full-size 104-key ANSI |
| `template-keyboard-tkl` | TKL 87-key ANSI |
| `template-keyboard-75pct` | 75% ~84-key |
| `template-keyboard-65pct` | 65% ~68-key |
| `template-keyboard-60pct` | 60% ~61-key |
| `template-keyboard-split` | Split ergonomic (generic) |
| `template-mouse-right` | Right-handed mouse |
| `template-mouse-ambidextrous` | Ambidextrous mouse |
| `template-capture-card-usb` | USB capture card (generic) |
| `template-usb-hub-4port` | 4-port USB hub |
| `template-thunderbolt-dock` | Thunderbolt dock (generic) |
| `template-monitor-27` | 27" 16:9 monitor |
| `template-monitor-34uw` | 34" 21:9 ultrawide |
| `template-webcam-usb` | USB webcam (generic) |
| `template-microphone-usb` | USB condenser microphone |
| `template-speaker-bookshelf` | Bookshelf speaker pair |

### 15.4 Category-specific blocks

Each block adds type-specific detail. All fields within each block are optional.

**KeyboardSpec**:

```yaml
KeyboardSpec:
  layout: string?               # "full", "tkl", "75pct", "65pct", "60pct", "40pct",
                                # "split", "ortho", "alice", "hhkb"
  standard: string?             # "ansi", "iso", "jis"
  key_count: uint?
  switch_type: string?          # "mechanical", "membrane", "topre", "optical", "hot-swap"
  switch_mount: string?         # "mx", "alps", "choc", "topre"
  wireless: string[]?           # ["bluetooth", "usb_2_4ghz", "none"]
  battery_mah: uint?
  via_compatible: bool?         # supports VIA/QMK configuration
  nkro: bool?                   # N-key rollover
  polling_rate_hz: uint?        # USB polling rate
  key_positions: ViaLayout?     # imported from VIA layout JSON
```

**AudioSpec** (microphones, speakers, headphones, audio interfaces):

```yaml
AudioSpec:
  device_class: string?         # "microphone", "speaker", "headphone", "interface",
                                # "dac", "amp", "dac_amp", "preamp"
  driver_type: string?          # "dynamic", "condenser", "ribbon", "planar",
                                # "electrostatic", "balanced_armature", "bone_conduction"
  polar_pattern: string?        # "cardioid", "omnidirectional", "figure_8",
                                # "supercardioid", "shotgun", "multi_pattern"
  frequency_response: FrequencyResponse?  # measured or specified response curve
  impedance_ohm: float?
  sensitivity_dbv: float?       # dB/V or dB/mW depending on device_class
  max_spl_db: float?            # maximum SPL
  self_noise_dba: float?        # self-noise (microphones)
  sample_rates: uint[]?         # supported sample rates [44100, 48000, 96000, 192000]
  bit_depths: uint[]?           # supported bit depths [16, 24, 32]
  channels: uint?               # number of channels
  phantom_power: bool?          # requires 48V phantom power
  inputs: uint?                 # number of inputs (audio interfaces)
  outputs: uint?                # number of outputs
  midi: bool?                   # has MIDI I/O
  loopback: bool?               # hardware loopback capability
  dsp: bool?                    # built-in DSP
  power_watts: float?           # amplifier power (speakers)
  driver_size_mm: float?        # speaker/headphone driver diameter

  # Speaker spatial characteristics (for spatial audio — see §2.13)
  speaker: SpeakerSpatialSpec?

SpeakerSpatialSpec:
  speaker_type: string?         # "bookshelf", "floorstanding", "satellite", "subwoofer",
                                # "soundbar", "ceiling", "in_wall", "on_wall", "portable"
  driver_count: uint?           # number of drivers (woofer + tweeter + mid = 3)
  driver_config: string?        # "2-way", "3-way", "coaxial", "full_range", "horn"
  enclosure: string?            # "sealed", "ported", "open_baffle", "transmission_line"
  dispersion: DispersionSpec?   # how sound radiates from this speaker
  crossover_hz: float[]?        # crossover frequencies between drivers
  bass_extension_hz: float?     # -6dB low-frequency point
  max_spl_db: float?            # maximum output SPL at 1m
  is_active: bool?              # powered speaker (built-in amplifier)
  is_powered_sub: bool?         # subwoofer with built-in amp + crossover
  sub_crossover_hz: float?      # built-in crossover frequency (if sub)

DispersionSpec:
  horizontal_deg: float?        # horizontal dispersion angle (-6dB, typically 60–120°)
  vertical_deg: float?          # vertical dispersion angle (-6dB, typically 30–60°)
  directivity_index_db: float?  # DI at 1kHz (higher = more focused)
  measurement_freq_hz: float?   # frequency at which dispersion was measured
  beamwidth_data: BeamwidthPoint[]?  # frequency-dependent dispersion if available

BeamwidthPoint:
  hz: float
  horizontal_deg: float
  vertical_deg: float

FrequencyResponse:
  type: string                  # "measured", "specified", "community"
  data_points: FreqPoint[]?     # frequency → dB pairs
  data_url: string?             # URL to downloadable response data (CSV, REW, AutoEQ)
  range_hz: { min: float, max: float }?  # specified frequency range
  flatness_db: float?           # ±dB deviation from flat (if specified)
  calibration_source: string?   # what tool/method was used to measure

FreqPoint:
  hz: float
  db: float
```

**HubSpec / DockSpec** (the topology data that makes routing smarter):

```yaml
HubSpec:
  ports: HubPort[]
  upstream_speed: string?       # "usb2", "usb3_5gbps", "usb3_10gbps", "usb3_20gbps"
  internal_hub_speed: string?   # actual internal hub chipset speed (often USB 2.0!)
  chipset: string?              # hub chipset if known (e.g., "VIA VL817", "Genesys GL3523")
  power_per_port_ma: uint?      # per-port current budget
  total_power_ma: uint?         # total hub power budget

HubPort:
  id: string
  speed: string                 # "usb2", "usb3_5gbps", "usb3_10gbps"
  type: string?                 # "type_a", "type_c", "internal"
  power_ma: uint?               # port power budget
  always_on: bool?              # powered when host is off?

DockSpec:
  thunderbolt_version: uint?    # 3, 4, 5
  upstream_bandwidth_gbps: float?  # total upstream bandwidth
  hub: HubSpec?                 # internal USB hub
  ethernet: EthernetSpec?       # internal ethernet adapter
  display_outputs: DisplayOutput[]?  # DP/HDMI outputs
  audio: AudioSpec?             # internal audio codec
  sd_card: bool?                # SD card reader
  internal_topology_notes: string?  # free-text notes about internal routing

EthernetSpec:
  speed_mbps: uint              # 100, 1000, 2500, 5000
  chipset: string?              # e.g., "Realtek RTL8153"
  internal_bus: string?         # how it connects internally ("usb3", "pcie")

DisplayOutput:
  connector: string             # "dp", "hdmi", "usb_c_dp_alt"
  version: string?              # "dp_1.4", "hdmi_2.1"
  max_resolution: Resolution?
  max_refresh: float?
  dsc: bool?                    # Display Stream Compression
  internal_bus: string?         # "thunderbolt_native", "dp_mst", "usb_c_dp_alt"
```

This is the data that solves the Thunderbolt dock opacity problem. When a
community member discovers that a CalDigit TS4 dock uses a `GL3523` hub
internally with USB 3.0 on ports 1–3 but USB 2.0 on ports 4–5, they add it
to the database. Every Ozma user with that dock immediately gets better
routing — the router knows not to put a USB 3.0 capture card on port 5.

**CaptureSpec**:

```yaml
CaptureSpec:
  interface: string?            # "usb", "pcie", "thunderbolt"
  max_input_resolution: Resolution?
  max_input_refresh: float?
  supported_inputs: string[]?   # ["hdmi_2.0", "dp_1.4", "sdi"]
  output_formats: string[]?     # ["mjpeg", "raw_yuyv", "raw_nv12", "h264"]
  passthrough: bool?            # has HDMI passthrough output
  hdcp: bool?                   # handles HDCP (and strips it)
  loop_out: bool?               # has loop-out port
  audio_capture: bool?          # captures embedded audio
  chipset: string?              # capture chipset (e.g., "MacroSilicon MS2109")
  usb_speed: string?            # actual USB speed class (not just what it claims)
  known_issues: string[]?       # known problems (e.g., "drops to USB 2.0 on some hubs")
```

**DisplaySpec** (monitors, projectors):

```yaml
DisplaySpec:
  # --- Panel ---
  panel_type: string?           # "ips", "va", "tn", "oled", "mini_led", "micro_led", "qd_oled"
  size_inches: float?
  aspect_ratio: string?         # "16:9", "21:9", "32:9", "16:10", "4:3", "3:2"
  native_resolution: Resolution?
  max_refresh: float?
  hdr: string?                  # "hdr400", "hdr600", "hdr1000", "hdr1400", "dolby_vision",
                                # "hdr10", "hdr10_plus"
  color_gamut: string?          # "srgb_100", "dci_p3_95", "adobe_rgb_99", "bt2020_80"
  color_depth_bits: uint?       # panel bit depth (8, 10, 12)
  frc: bool?                    # frame rate control (8-bit+FRC ≠ true 10-bit)
  response_time_ms: float?      # GtG
  black_frame_insertion: bool?
  vrr: VrrSpec?                 # variable refresh rate support

  # --- Physical ---
  bezel_mm: BezelSpec?
  active_area_mm: Dimensions?   # actual screen area (w, h)
  weight_kg: float?             # panel weight (without stand)
  weight_with_stand_kg: float?
  vesa: string?                 # "75x75", "100x100", "200x200", "300x300"
  curve_radius_mm: uint?        # for curved panels (1000R, 1800R, etc.)
  pivot: bool?                  # can rotate to portrait
  tilt_range_deg: { min: float, max: float }?
  height_adjust_mm: float?      # height adjustment range
  swivel_range_deg: float?

  # --- Inputs (the monitor's port topology) ---
  inputs: DisplayInput[]
  active_input: uint?           # currently selected input (if known)

  # --- Built-in features (monitor as compound device) ---
  speakers: DisplaySpeakerSpec?       # built-in speakers (not just bool)
  usb_hub: HubSpec?                   # built-in USB hub
  kvm_switch: DisplayKvmSpec?         # built-in KVM switch
  usb_c_power_delivery: UsbPdSpec?    # USB-C upstream power delivery
  microphone: bool?                   # built-in microphone
  webcam: bool?                       # built-in camera
  ambient_light_sensor: bool?         # auto-brightness sensor
  proximity_sensor: bool?             # presence detection
  pip_pbp: PipPbpSpec?                # picture-in-picture / picture-by-picture

  # --- Control methods ---
  control: DisplayControlSpec

VrrSpec:
  type: string?                 # "freesync", "freesync_premium", "freesync_premium_pro",
                                # "gsync", "gsync_compatible", "gsync_ultimate", "adaptive_sync"
  range_hz: { min: float, max: float }?  # VRR frequency range (e.g., 48–165 Hz)
  lfc: bool?                    # low framerate compensation

BezelSpec:
  top_mm: float?
  bottom_mm: float?
  left_mm: float?
  right_mm: float?
  uniform: bool?                # all bezels same width (for tiling calculations)

DisplayInput:
  id: string                    # unique input identifier ("hdmi_1", "dp_1", "usb_c_1")
  connector: string             # "hdmi", "dp", "mini_dp", "usb_c", "vga", "dvi_d", "dvi_i"
  version: string?              # "hdmi_1.4", "hdmi_2.0", "hdmi_2.1", "dp_1.2", "dp_1.4", "dp_2.1"
  max_resolution: Resolution?   # maximum resolution on this specific input
  max_refresh: float?           # maximum refresh on this input (may differ per input)
  max_bandwidth_gbps: float?    # maximum link bandwidth (HDMI 2.1 = 48 Gbps, DP 1.4 = 32.4 Gbps)
  hdr_supported: bool?          # HDR on this input
  hdcp: string?                 # "hdcp_1.4", "hdcp_2.2", "hdcp_2.3"
  arc: bool?                    # HDMI ARC support
  earc: bool?                   # HDMI eARC support
  cec: bool?                    # HDMI CEC support on this input
  dsc: bool?                    # Display Stream Compression (enables higher res/refresh)
  input_number: uint?           # OSD/DDC input number (for input switching commands)
  usb_c_features: UsbCDisplayFeatures?  # if USB-C: what else this port carries
  physical: PhysicalPortInfo?   # where on the monitor body

UsbCDisplayFeatures:
  dp_alt_mode: bool?            # carries DisplayPort Alt Mode
  dp_version: string?           # "dp_1.4", "dp_2.0"
  usb_data: bool?               # carries USB data (to built-in hub)
  usb_data_speed: string?       # "usb2", "usb3_5gbps", "usb3_10gbps"
  power_delivery_w: float?      # watts delivered upstream to connected device
  pd_spec: UsbPdSpec?           # full PD negotiation capabilities
  # A single USB-C port on a monitor might carry: DP 1.4 video + USB 3.0 data
  # to the built-in hub + 90W PD charging. The spec captures all of this.

DisplaySpeakerSpec:
  count: uint?                  # number of speakers (typically 2)
  power_watts: float?           # per-speaker or total power
  frequency_response: FrequencyResponse?  # if known
  volume_control: string?       # "ddc_ci", "osd_only", "none"
  # Built-in monitor speakers are audio sinks in the routing graph.
  # They can receive audio via HDMI/DP embedded audio, ARC, or USB audio.

DisplayKvmSpec:
  # Many monitors have a built-in KVM switch — they switch their USB hub's
  # upstream between multiple inputs. When you select HDMI 1, the USB hub
  # connects to the PC on HDMI 1. Select HDMI 2, USB hub switches to PC 2.
  # This is a switch device (§2.5) embedded inside the monitor.
  type: string                  # "auto" (follows video input), "manual" (separate button),
                                # "hotkey" (keyboard shortcut), "none"
  usb_upstream_ports: uint?     # number of USB-B/USB-C upstream connections
  auto_follows_input: bool?     # KVM switches when video input switches
  independent_switching: bool?  # KVM can be switched independently of video input
  controllable: bool?           # can the KVM be switched via DDC/CI or USB command
  # The monitor's built-in KVM is modelled as a switch (§2.5) with
  # controllability derived from this spec. If controllable, the router
  # can switch the monitor's KVM as part of pipeline activation.

PipPbpSpec:
  pip: bool?                    # picture-in-picture support
  pbp: bool?                    # picture-by-picture (side-by-side)
  max_sources: uint?            # how many simultaneous inputs (typically 2–4)
  controllable: bool?           # can PIP/PBP be controlled via DDC/CI
  pip_sizes: string[]?          # ["small", "medium", "large"]
  pbp_layouts: string[]?        # ["50_50", "70_30", "triple"]

DisplayControlSpec:
  # How the monitor can be controlled — this determines the control path (§2.12)
  methods: DisplayControlMethod[]
  osd_lockout: bool?            # can the OSD be locked via DDC/CI (prevent physical tampering)

DisplayControlMethod:
  type: string                  # control method type (see table below)
  capabilities: string[]        # what can be controlled via this method
  bidirectional: bool           # can we read state back?
  notes: string?

# Control method types:
#
# | Method | Bidirectional | Typical capabilities | Notes |
# |--------|--------------|---------------------|-------|
# | ddc_ci | Yes (read+write) | brightness, contrast, input, volume, power, color temp, OSD lock | Primary electronic control. Via I2C over display cable. Only works from the connected PC. |
# | osd_buttons | No (write only, no feedback) | everything (via menu navigation) | Physical buttons on monitor. Manual only. |
# | osd_joystick | No | everything (via menu) | Joystick/nub on monitor. Manual only. |
# | ir_remote | No (write only) | input, volume, power, brightness, PIP/PBP | IR remote control. Some monitors include one. Write-only. |
# | usb_control | Yes | brightness, input, KVM, firmware update | Vendor-specific USB HID commands. Monitor-specific driver needed. |
# | cec | Yes (HDMI CEC) | power, input, volume, OSD | Via HDMI CEC (pin 13). Only on HDMI inputs. |
# | network | Yes (IP) | everything | Smart monitors with IP management (enterprise, digital signage). Rare. |
# | bluetooth | Varies | power, input, settings | Some Samsung/LG monitors. Vendor app required. |
# | vendor_app | Varies | everything | Desktop app (Dell Display Manager, LG OnScreen Control, Samsung Easy Setting Box). Runs on connected PC. |
```

**Example — a modern 27" monitor as a compound device in the database**:

```yaml
id: "dell-u2723qe"
type: display
name: "Dell U2723QE"
vendor: "Dell"
display:
  panel_type: "ips"
  size_inches: 27
  aspect_ratio: "16:9"
  native_resolution: { w: 3840, h: 2160 }
  max_refresh: 60
  hdr: "hdr400"
  color_gamut: "dci_p3_98"
  color_depth_bits: 10
  frc: false

  inputs:
    - id: hdmi_1
      connector: hdmi
      version: "hdmi_2.0"
      max_resolution: { w: 3840, h: 2160 }
      max_refresh: 60
      hdcp: "hdcp_2.2"
      input_number: 1
      physical: { position: { face: rear, row: 0, column: 0 } }

    - id: dp_1
      connector: dp
      version: "dp_1.4"
      max_resolution: { w: 3840, h: 2160 }
      max_refresh: 60
      dsc: true
      hdcp: "hdcp_2.2"
      input_number: 2
      physical: { position: { face: rear, row: 0, column: 1 } }

    - id: usb_c_1
      connector: usb_c
      version: "dp_1.4"
      max_resolution: { w: 3840, h: 2160 }
      max_refresh: 60
      dsc: true
      input_number: 3
      usb_c_features:
        dp_alt_mode: true
        dp_version: "dp_1.4"
        usb_data: true
        usb_data_speed: "usb3_10gbps"
        power_delivery_w: 90
      physical: { position: { face: rear, row: 0, column: 2 } }

  speakers:
    count: 2
    power_watts: 5

  usb_hub:
    ports:
      - { id: "usb_a_1", speed: "usb3_10gbps", type: "type_a" }
      - { id: "usb_a_2", speed: "usb3_10gbps", type: "type_a" }
      - { id: "usb_a_3", speed: "usb3_5gbps", type: "type_a" }
      - { id: "usb_c_downstream", speed: "usb3_10gbps", type: "type_c" }
    upstream_speed: "usb3_10gbps"
    # Hub upstream connects to whichever USB-C/USB-B upstream is active

  kvm_switch:
    type: "auto"
    usb_upstream_ports: 2           # USB-C input + USB-B upstream
    auto_follows_input: true        # USB follows video input
    independent_switching: false
    controllable: true              # via DDC/CI custom command

  usb_c_power_delivery:
    source_pdos:
      - { type: "fixed", voltage_v: 5, current_ma: 3000 }
      - { type: "fixed", voltage_v: 9, current_ma: 3000 }
      - { type: "fixed", voltage_v: 15, current_ma: 3000 }
      - { type: "fixed", voltage_v: 20, current_ma: 4500 }  # 90W

  control:
    methods:
      - type: ddc_ci
        capabilities: ["brightness", "contrast", "input_select", "volume", "power",
                       "color_temp", "kvm_switch"]
        bidirectional: true
      - type: osd_joystick
        capabilities: ["everything"]
        bidirectional: false
      - type: vendor_app
        capabilities: ["everything", "firmware_update", "window_layout"]
        bidirectional: true
        notes: "Dell Display Manager 2.0 (DDM)"

  bezel_mm: { top: 7.5, bottom: 7.5, left: 7.5, right: 7.5, uniform: true }
  vesa: "100x100"
  pivot: true
  tilt_range_deg: { min: -5, max: 21 }
  height_adjust_mm: 150
  swivel_range_deg: 45
```

**Why this level of detail matters**:

1. **Input switching**: The router knows this monitor has HDMI, DP, and USB-C
   inputs, each with different capabilities (USB-C carries USB data + 90W PD,
   HDMI doesn't). When switching scenarios, the router can command input
   switching via DDC/CI as part of pipeline activation.

2. **Built-in KVM**: The monitor's USB hub follows the video input — when Ozma
   switches the monitor to HDMI 1 (connected to PC A), the USB hub also
   switches. The router knows this and doesn't need to switch the USB hub
   separately. This is modelled as a switch (§2.5) with `auto_follows_input`.

3. **USB-C as compound port**: A single USB-C cable carries 4K60 video + USB
   3.0 data to the hub + 90W charging. The spec decomposes this into its
   component functions so the router can reason about each independently —
   video bandwidth, USB data bandwidth, and power delivery are separate
   concerns that happen to share a connector.

4. **Control path selection**: The router knows this monitor can be controlled
   via DDC/CI (bidirectional, from connected PC), OSD joystick (manual only),
   or Dell Display Manager (app on connected PC). The control path (§2.12) is:
   DDC/CI via desktop agent on the connected PC → monitor. If the agent is
   offline, control falls back to manual.

5. **PIP/PBP for monitoring**: Some monitors show two inputs simultaneously.
   The router can use this for KVM preview — show the active machine full
   screen and the other machine in PIP. If the monitor supports DDC/CI
   PIP control, this is automated.

6. **Physical port location**: "Plug the Ozma node into `usb_c_1` (rear,
   third from left) — it carries video + USB + 90W power in one cable."
```

**CameraSpec**:

```yaml
CameraSpec:
  camera_type: string?          # "webcam", "ip_camera", "ptz", "doorbell", "action"
  sensor: string?               # sensor model
  resolutions: Resolution[]?    # supported resolutions
  max_framerate: float?
  fov_degrees: float?           # field of view
  autofocus: bool?
  optical_zoom: float?          # optical zoom range
  night_vision: bool?
  two_way_audio: bool?
  ptz: PtzSpec?
  onvif: bool?                  # ONVIF compatible
  rtsp_url_pattern: string?     # URL template for RTSP stream
  local_storage: bool?          # SD card recording
  poe: bool?                    # Power over Ethernet

PtzSpec:
  pan_range: { min: float, max: float }?   # degrees
  tilt_range: { min: float, max: float }?
  zoom_range: { min: float, max: float }?
  presets: uint?                # number of preset positions
  speed: float?                 # degrees per second
```

**ActuatorSpec**:

```yaml
ActuatorSpec:
  actuator_type: string?        # "desk", "monitor_arm", "crane", "linear"
  axes: ActuatorAxisSpec[]?
  controller: string?           # control protocol ("ble", "serial", "mqtt", "http")
  brand: string?                # e.g., "Jarvis", "Uplift", "FlexiSpot"
  integration: string?          # how to control it ("jarvis_ble", "generic_serial", "http_api")

ActuatorAxisSpec:
  name: string                  # "height", "tilt", "pan"
  range: { min: float, max: float }
  unit: string                  # "mm", "degrees"
  speed: float?                 # movement speed
  presets: { name: string, value: float }[]?
```

**PcieCardSpec** (any PCIe expansion card — the bus-level properties):

```yaml
PcieCardSpec:
  generation: uint?             # 3, 4, 5
  lanes: uint?                  # 1, 4, 8, 16
  form_factor: string?          # "full_height", "half_height", "low_profile", "m2", "u2"
  slot_type: string?            # "x16", "x8", "x4", "x1", "m2_m_key", "m2_b_key"
  power_draw_w: float?          # TDP or measured power draw
  bifurcation: string?          # if the card splits lanes ("x8x8", "x4x4x4x4")
  subsystem: string[]?          # what the card provides: ["gpu", "nic", "capture", "usb_controller",
                                # "nvme", "sata", "thunderbolt", "sound_card", "fpga"]
```

A PCIe card is often compound — a GPU has display outputs, encode/decode
engines, and sometimes a USB-C port with DP alt mode. A Thunderbolt add-in
card has a USB controller, a DP input, and a Thunderbolt port. The `subsystem`
field lists what the card provides, and the detailed capabilities live in
the relevant category blocks (`gpu`, `capture`, `hub`, etc.) on the same entry.

**GpuSpec** (GPU-specific capabilities — critical for routing encode/decode):

```yaml
GpuSpec:
  vendor: string?               # "nvidia", "amd", "intel", "apple"
  architecture: string?         # "ada_lovelace", "rdna3", "arc_alchemist", "m3"
  vram_mb: uint?
  vram_type: string?            # "gddr6x", "gddr6", "hbm3"
  vram_bus_width: uint?         # memory bus width (128, 192, 256, 384 bit)
  display_outputs: DisplayOutput[]?  # physical display connectors
  display_engine: DisplayEngineSpec?  # internal display pipeline constraints
  encode: GpuCodecCapability?   # hardware encoder (NVENC, VCN, QSV, VideoToolbox)
  decode: GpuCodecCapability?   # hardware decoder
  compute_units: uint?          # shader cores / compute units
  ray_tracing: bool?            # hardware ray tracing cores
  tdp_w: float?
  driver_version: string?       # currently installed driver

GpuCodecCapability:
  engine: string?               # "nvenc", "vcn", "qsv", "videotoolbox", "vaapi",
                                # "rkmpp", "v4l2m2m"
  max_encode_sessions: uint?    # simultaneous encode sessions
  max_decode_sessions: uint?    # simultaneous decode sessions (often unlimited)
  max_total_sessions: uint?     # combined encode+decode limit (if shared engine)
  session_limit_source: string? # "hardware", "driver" (NVIDIA consumer = driver-limited)
  independent_engines: bool?    # can encode and decode run simultaneously without contention?
  shared_with: string[]?        # other subsystems sharing this engine ("npu", "vpp", "jpeg")
  codecs: GpuCodecDetail[]?

GpuCodecDetail:
  codec: string                 # "h264", "h265", "av1", "vp9", "vp8", "jpeg", "mpeg2"
  direction: string             # "encode", "decode", "both"
  max_resolution: Resolution?
  max_framerate: float?         # at max resolution
  max_framerate_at_1080p: float? # many encoders handle higher fps at lower res
  max_framerate_at_4k: float?
  profiles: string[]?           # ["main", "high", "main10", "main_444"]
  bit_depth: uint[]?            # [8, 10, 12]
  chroma: string[]?             # ["4:2:0", "4:2:2", "4:4:4"]
  b_frames: bool?               # B-frame support (encode only)
  lookahead: bool?              # lookahead support (encode only)
  max_bitrate_mbps: float?
  latency_ms: float?            # typical encode/decode latency at default settings
  low_latency_mode: bool?       # supports ultra-low-latency mode (disables B-frames, lookahead)
  low_latency_ms: float?        # latency in low-latency mode
  quality_presets: string[]?    # ["p1_fastest", "p4_medium", "p7_slowest"] (NVENC)
                                # or ["speed", "balanced", "quality"] (QSV/VCN)
  session_limit_override: uint? # per-codec session limit (if different from engine max)
```

**Encode/decode session limits in practice**:

| GPU | Engine | Max encode | Max decode | Limit type | Notes |
|-----|--------|-----------|-----------|-----------|-------|
| NVIDIA GeForce (pre-2023) | NVENC | 3 | Unlimited | Driver | Historic limit |
| NVIDIA GeForce (2023) | NVENC | 5 | Unlimited | Driver | Raised March 2023 |
| NVIDIA GeForce (2024) | NVENC | 8 | Unlimited | Driver | Raised early 2024 |
| NVIDIA GeForce (late 2025+) | NVENC | 12 | Unlimited | Driver | Raised ~Dec 2025 |
| NVIDIA Quadro/RTX Pro/L-series | NVENC | Unlimited | Unlimited | None | Same silicon, no driver cap |
| NVIDIA (any) + NVDEC | NVDEC | — | Unlimited | Hardware | Decode engine is separate, limited by GPU power not session count |
| AMD RX 7000 | VCN 4.0 | 4 | Unlimited | Hardware | Per-engine; some SKUs have 2 VCN engines (= 8 total) |
| AMD RX 9000 | VCN 5.0 | TBD | Unlimited | Hardware | |
| Intel Arc | Xe media engine | Unlimited | Unlimited | Hardware | AV1 encode at hardware speed |
| Intel iGPU (12th+ gen) | Quick Sync | Unlimited | Unlimited | Hardware | Independent of dGPU |
| Apple M-series | VideoToolbox | 4–8 | Unlimited | Hardware | Varies by chip; media engine shared with ProRes |
| Rockchip RK3588 | MPP | 4 | 8 | Hardware | Separate encode/decode engines |

Note: NVIDIA consumer session limits are driver-enforced and have been
steadily increasing (2→3→5→8→12 over 2020–2025). The hardware has no
inherent session cap — the limit differentiates consumer from professional
cards. The device database should track the limit per driver version, not
just per GPU model, since a driver update changes the limit on existing
hardware. Performance is the real constraint at high session counts — 8
concurrent 4K60 NVENC sessions may exceed the encoder's throughput even
though the driver allows them. The router should track actual encoder
utilisation (§2.7) alongside the session count.

**iGPU + dGPU simultaneously**: On a system with both an Intel iGPU (Quick
Sync) and an NVIDIA dGPU (NVENC), both encode engines are available
independently. The router can use Quick Sync for one encode job and NVENC
for another, doubling the total encode capacity. The `gpu` block on the CPU
entry (§15 `CpuSpec.igpu`) and the discrete GPU entry both contribute
encode capabilities to the same machine's codec pool.

**CPU software encoding** (not GPU — but part of the same codec pool):

Software encoders (x264, x265, SVT-AV1, libvpx) run on CPU cores. They
don't have session limits — they're limited by CPU capacity (§2.7). The
codec plugin (§6.3) models them as `software_codec` devices with resource
costs:

| Encoder | Resolution | CPU cost (typical) | Latency | Quality vs HW |
|---------|-----------|-------------------|---------|---------------|
| x264 (ultrafast) | 1080p30 | 10–20% (4-core) | 5–10ms | Lower |
| x264 (medium) | 1080p30 | 30–50% (4-core) | 20–40ms | Higher |
| x265 (ultrafast) | 1080p30 | 20–40% (4-core) | 10–20ms | Lower |
| SVT-AV1 (preset 10) | 1080p30 | 15–30% (4-core) | 10–20ms | Good |
| SVT-AV1 (preset 4) | 1080p30 | 60–90% (4-core) | 50–100ms | Excellent |

The router selects between hardware and software encoding based on the
intent, available hardware sessions, and CPU headroom. `gaming` intent
prefers hardware (low latency). `broadcast` intent may prefer software at
a slower preset (higher quality, latency doesn't matter). If all NVENC
sessions are in use, the router falls back to CPU encoding — and accounts
for the CPU resource cost (§2.7).

**NPU/media engine contention**: On some platforms, the video encode engine
shares silicon with other subsystems:

| Platform | Shared between | Impact |
|----------|---------------|--------|
| Intel (12th+ gen) | Quick Sync + AI inference (OpenVINO) | Heavy AI workload reduces encode throughput |
| AMD APU (Ryzen AI) | VCN + XDNA NPU | Encode and NPU inference may contend for memory bandwidth |
| Apple M-series | VideoToolbox + ProRes + Neural Engine | ProRes encode/decode shares media engine time |
| Rockchip RK3588 | MPP + RKNN NPU | Separate engines but shared memory bus |

The `shared_with` field on `GpuCodecCapability` expresses this. When the
router knows that Quick Sync shares resources with OpenVINO inference, it
can predict that running Frigate AI detection (on the same iGPU) will
reduce available encode performance — and route encoding to the dGPU or
CPU instead.

This feeds directly into the codec plugin (§6.3) and the cost model (§8.1) —
the router treats each encode session as a resource on the device, with
known capacity limits and contention with other subsystems.

**DisplayEngineSpec** — the internal display pipeline that determines which
combinations of outputs actually work simultaneously:

```yaml
DisplayEngineSpec:
  heads: uint                   # display heads (independent framebuffers/CRTCs)
  clock_sources: uint?          # independent pixel clock generators (PLLs)
  max_total_pixel_clock_mhz: float?  # aggregate pixel clock budget across all outputs
  max_per_head_pixel_clock_mhz: float?  # maximum pixel clock per individual head
  max_total_bandwidth_gbps: float?  # total display output bandwidth (all links combined)
  output_links: GpuOutputLink[]  # internal link from head/clock to physical connector
  constraints: DisplayConstraint[]?  # combinations that don't work

GpuOutputLink:
  output_id: string             # physical output ("dp_1", "hdmi_1", "usb_c_1")
  connector: string             # "dp", "hdmi", "usb_c", "vga", "dvi"
  link_type: string             # "dp_mst", "dp_sst", "hdmi_tmds", "hdmi_frl"
  max_link_bandwidth_gbps: float?  # max bandwidth of this specific output link
  head_assignment: string?      # which head(s) can drive this output ("any", "head_0", "head_0_or_1")
  clock_source: string?         # which PLL drives this output ("pll_0", "pll_1", "shared")
  mst_capable: bool?            # can this output daisy-chain via MST
  mst_max_streams: uint?        # max MST streams from this output (DP MST hub)
  dsc: bool?                    # Display Stream Compression available on this link
  dsc_max_bpp: float?           # maximum compressed bits per pixel (e.g., 12 bpp)

DisplayConstraint:
  type: string                  # constraint type (see table below)
  description: string           # human-readable explanation
  outputs_affected: string[]    # which outputs are involved
  condition: string?            # when this constraint applies
```

**Why this matters — real-world GPU display constraints**:

GPUs don't simply have "4 outputs = 4 displays". The internal architecture
has heads (CRTCs), clock sources (PLLs), and link bandwidth that are shared
and limited in non-obvious ways:

**NVIDIA (Ada Lovelace / RTX 40 series)**:
```yaml
display_engine:
  heads: 4                      # 4 independent CRTCs
  clock_sources: 4              # 4 PLLs
  max_total_pixel_clock_mhz: 2380  # ~2.4 GHz total across all outputs
  output_links:
    - { output_id: dp_1, link_type: dp_sst, max_link_bandwidth_gbps: 32.4, dsc: true }
    - { output_id: dp_2, link_type: dp_sst, max_link_bandwidth_gbps: 32.4, dsc: true }
    - { output_id: dp_3, link_type: dp_sst, max_link_bandwidth_gbps: 32.4, dsc: true }
    - { output_id: hdmi_1, link_type: hdmi_frl, max_link_bandwidth_gbps: 48 }
  constraints:
    - type: max_simultaneous
      description: "Maximum 4 displays simultaneously"
      outputs_affected: ["dp_1", "dp_2", "dp_3", "hdmi_1"]
    - type: pixel_clock_shared
      description: "4K 144Hz on HDMI uses significant pixel clock budget — may limit other outputs to lower refresh"
      outputs_affected: ["hdmi_1"]
      condition: "hdmi_1 resolution > 4K60"
```

**NVIDIA (Turing / RTX 20 series)**:
```yaml
display_engine:
  heads: 4
  clock_sources: 2              # only 2 PLLs! This is the key constraint.
  output_links:
    - { output_id: dp_1, link_type: dp_sst, max_link_bandwidth_gbps: 25.92 }
    - { output_id: dp_2, link_type: dp_sst, max_link_bandwidth_gbps: 25.92 }
    - { output_id: dp_3, link_type: dp_sst, max_link_bandwidth_gbps: 25.92 }
    - { output_id: hdmi_1, link_type: hdmi_tmds, max_link_bandwidth_gbps: 18 }
  constraints:
    - type: clock_sharing
      description: "2 PLLs shared across 4 outputs — displays sharing a PLL must use compatible pixel clocks"
      outputs_affected: ["dp_1", "dp_2", "dp_3", "hdmi_1"]
      # Two 4K 60Hz displays at different refresh rates (e.g., 60Hz + 144Hz)
      # need different pixel clocks. If they land on the same PLL, one must
      # downclock. The driver handles this silently — the user just sees
      # their 144Hz monitor running at 120Hz and doesn't know why.
    - type: hdmi_bandwidth
      description: "HDMI 2.0b — max 4K60 4:2:0 or 4K30 4:4:4"
      outputs_affected: ["hdmi_1"]
```

**AMD (RDNA 3 / RX 7000 series)**:
```yaml
display_engine:
  heads: 4
  clock_sources: 4              # 4 PLLs — more flexible than Turing
  max_total_bandwidth_gbps: 80  # but total DP bandwidth is shared
  output_links:
    - { output_id: dp_1, link_type: dp_sst, max_link_bandwidth_gbps: 40, dsc: true }
    - { output_id: dp_2, link_type: dp_sst, max_link_bandwidth_gbps: 40, dsc: true }
    - { output_id: hdmi_1, link_type: hdmi_frl, max_link_bandwidth_gbps: 48 }
    - { output_id: usb_c_1, link_type: dp_sst, max_link_bandwidth_gbps: 40, dsc: true }
  constraints:
    - type: bandwidth_shared
      description: "DP outputs share total bandwidth pool — two 4K 144Hz outputs may exceed aggregate limit without DSC"
      outputs_affected: ["dp_1", "dp_2", "usb_c_1"]
    - type: usb_c_mode
      description: "USB-C port operates in DP Alt Mode — disables USB data when active as display output"
      outputs_affected: ["usb_c_1"]
```

**Intel (Alder/Raptor Lake iGPU)**:
```yaml
display_engine:
  heads: 4
  clock_sources: 3              # 3 PLLs (DPLL0, DPLL1, DPLL4)
  output_links:
    - { output_id: dp_1, link_type: dp_sst, max_link_bandwidth_gbps: 32.4 }
    - { output_id: hdmi_1, link_type: hdmi_tmds, max_link_bandwidth_gbps: 18 }
    - { output_id: dp_2, link_type: dp_sst, max_link_bandwidth_gbps: 32.4 }
  constraints:
    - type: max_simultaneous
      description: "iGPU supports max 3 simultaneous displays (even though 4 heads exist)"
      outputs_affected: ["dp_1", "hdmi_1", "dp_2"]
    - type: shared_pll
      description: "HDMI and DP1 share DPLL0 — incompatible pixel clocks force one to lower refresh"
      outputs_affected: ["dp_1", "hdmi_1"]
```

**How the router uses display engine constraints**:

1. **Validating display configurations**: Before recommending a multi-monitor
   setup, the router checks whether the GPU can actually drive it. "3× 4K
   144Hz" may require DSC to fit within bandwidth limits. "4K 144Hz HDMI +
   4K 60Hz DP" may conflict on a shared PLL.

2. **Recommending which output to use**: "Connect your 144Hz monitor to DP 1,
   not HDMI — HDMI 2.0 can't carry 4K 144Hz without chroma subsampling."

3. **Explaining why a display isn't at full refresh**: "Your monitor is
   running at 120Hz instead of 144Hz because it shares a PLL with your
   other display. Move one monitor to a different output to use independent
   PLLs."

4. **DSC awareness**: "Your GPU can drive 3× 4K 144Hz but only with Display
   Stream Compression enabled. Your monitor supports DSC — this is handled
   automatically."

5. **MST hub detection**: If a DP output is feeding an MST hub (daisy-chain
   or external DP hub), the total bandwidth of the MST group comes from one
   output link. Two 4K60 displays on one DP MST hub need 2× the bandwidth
   of one — the router checks whether the link can carry it.

6. **GPU passthrough planning**: For VFIO GPU passthrough, the router knows
   the GPU's display capabilities and can recommend which outputs to pass
   through vs keep for the host. "Pass HDMI to the VM; keep DP 1 for the
   host's management console."

**Discovery**: Display engine capabilities can come from:
- Device database entry (`spec` quality) — known constraints per GPU model
- Driver query (`reported` quality) — `nvidia-smi`, `xrandr --verbose`,
  DRM/KMS `drmModeGetResources` for head/CRTC count
- Measured (`measured` quality) — attempt a configuration, observe if it
  succeeds or the driver downclocks

**NetworkCardSpec** (NICs, WiFi, Bluetooth — beyond the physical port):

```yaml
NetworkCardSpec:
  interface_type: string?       # "ethernet", "wifi", "bluetooth", "cellular"
  speed_mbps: uint?             # max link speed (1000, 2500, 10000)
  wifi_standard: string?        # "wifi5", "wifi6", "wifi6e", "wifi7"
  wifi_bands: string[]?         # ["2.4ghz", "5ghz", "6ghz"]
  bluetooth_version: string?    # "5.0", "5.3"
  chipset: string?              # "Intel I226-V", "Realtek RTL8125", "Intel AX211"
  poe: bool?                    # Power over Ethernet capable
  wake_on_lan: bool?
  sr_iov: bool?                 # SR-IOV virtualisation support
  dpdk: bool?                   # DPDK-compatible
  offload: string[]?            # ["tso", "lro", "rx_checksum", "tx_checksum"]
```

**StorageSpec** (SSDs, HDDs, NVMe — relevant for recording, replay buffer, backup):

```yaml
StorageSpec:
  interface: string?            # "nvme", "sata", "usb", "sd_card"
  form_factor: string?          # "m2_2280", "2.5_inch", "3.5_inch", "sd", "microsd"
  capacity_gb: uint?
  sequential_read_mbps: float?
  sequential_write_mbps: float?
  random_read_iops: uint?
  random_write_iops: uint?
  endurance_tbw: float?         # TBW rating
  type: string?                 # "nand_tlc", "nand_qlc", "nand_slc", "hdd_cmr", "hdd_smr"
  dram_cache: bool?
  power_loss_protection: bool?
```

Storage specs inform the router about recording capabilities — a node with
a slow SD card can't sustain 4K recording, but one with an NVMe drive can.
The replay buffer and session recording features use these specs to determine
achievable bitrates and buffer depths.

**NetworkSwitchSpec**:

```yaml
NetworkSwitchSpec:
  managed: bool?                # managed vs unmanaged
  port_count: uint?             # total Ethernet ports
  sfp_count: uint?              # SFP/SFP+ cage count
  port_speeds: uint[]?          # supported speeds in Mbps [100, 1000, 2500, 10000]
  backplane_gbps: float?        # switching backplane capacity
  blocking: string?             # "non_blocking", "blocking" (ratio in notes)
  poe: PoeSwitchSpec?           # PoE capability
  vlan: bool?                   # VLAN support
  igmp: bool?                   # IGMP snooping
  lacp: bool?                   # link aggregation
  stp: bool?                    # spanning tree
  management: string[]?         # ["snmp", "ssh", "http", "unifi", "mikrotik", "cli"]
  chipset: string?              # switching ASIC
  fan_count: uint?              # cooling fans (noise consideration)
  fanless: bool?                # passively cooled
  rack_mountable: bool?
  rack_units: uint?             # 1U, 2U

PoeSwitchSpec:
  standard: string[]?           # ["802.3af", "802.3at", "802.3bt"]
  total_budget_w: float?        # total PoE power budget across all ports
  per_port_max_w: float?        # maximum per-port PoE power
  poe_port_count: uint?         # how many ports have PoE (may be less than total)
```

**RouterSpec**:

```yaml
RouterSpec:
  wan_ports: uint?              # number of WAN interfaces
  wan_types: string[]?          # ["ethernet", "fibre_sfp", "dsl", "lte", "5g"]
  lan_ports: uint?
  wifi: WifiSpec?               # integrated WiFi (if any)
  throughput_mbps: float?       # rated routing throughput
  vpn_throughput_mbps: float?   # rated VPN throughput
  vpn_types: string[]?          # ["wireguard", "openvpn", "ipsec", "l2tp"]
  firewall: bool?
  ids_ips: bool?                # intrusion detection/prevention
  nat: bool?
  dhcp: bool?
  dns: bool?
  management: string[]?         # ["ssh", "http", "unifi", "openwrt", "mikrotik", "pfsense"]
  chipset: string?
  os: string?                   # "openwrt", "routeros", "pfsense", "opnsense", "unifi_os"

WifiSpec:
  standards: string[]?          # ["wifi5", "wifi6", "wifi6e", "wifi7"]
  bands: string[]?              # ["2.4ghz", "5ghz", "6ghz"]
  mimo: string?                 # "2x2", "4x4", "8x8"
  max_clients: uint?
  mesh: bool?                   # mesh networking support
  channels: uint[]?             # available channels
```

**AccessPointSpec**:

```yaml
AccessPointSpec:
  wifi: WifiSpec                # WiFi capabilities (see RouterSpec)
  poe_powered: bool?            # can be powered by PoE
  poe_standard: string?         # required PoE standard
  ceiling_mount: bool?
  wall_mount: bool?
  outdoor: bool?
  management: string[]?         # ["unifi", "omada", "standalone", "cloud"]
  mesh: bool?
  ethernet_uplink: string?      # "1g", "2.5g", "10g"
```

**FurnitureSpec** (desks, racks, stands, mounts — physical objects that contain
or support devices):

```yaml
FurnitureSpec:
  furniture_type: string?       # "desk", "rack", "stand", "mount", "shelf", "table",
                                # "chair", "sofa", "cart"
  surface_mm: Dimensions?       # usable surface area { w, d } (desks, tables, shelves)
  height_range_mm: HeightRange? # for sit/stand desks, adjustable mounts
  rack_units: uint?             # for server racks (e.g., 42)
  weight_capacity_kg: float?    # maximum load
  motorised: bool?              # has motorised adjustment
  integration: string?          # control protocol ("flexispot_serial", "linak_ble",
                                # "jarvis_serial", "generic_uart", "http")
  slots: FurnitureSlot[]?       # named positions for devices/items
  materials: string[]?          # ["oak", "steel", "glass", "mdf"]
  seating: SeatingSpec?         # for chairs/sofas

HeightRange:
  min_mm: float
  max_mm: float
  presets: { name: string, height_mm: float }[]?

FurnitureSlot:
  id: string                    # "surface", "under_desk_left", "cable_tray",
                                # "monitor_arm_1", "U12", "shelf_2"
  type: string?                 # what fits here ("device", "monitor", "rack_unit", "any")
  pos: Position3d               # position relative to furniture origin
  facing: string?               # direction ("up", "front", "left")
  capacity: string?             # size constraint ("2U", "max_27inch")

SeatingSpec:
  seats: uint                   # number of seats (1 for chair, 2–5 for sofa)
  adjustable_height: bool?
  headrest: bool?
  lumbar: bool?
  recline_range_deg: { min: float, max: float }?
  occupancy_sensor: string?     # "pressure_mat", "ble_beacon", "camera", "none"
```

**MotherboardSpec** (the most important compound device — maps every physical
port to its internal controller):

```yaml
MotherboardSpec:
  form_factor: string?          # "atx", "matx", "mitx", "dtx", "eatx", "sff",
                                # "laptop", "sbc" (Pi, OPi), "embedded"
  chipset_id: string?           # device database ref for the chipset
  cpu_socket: string?           # "lga1700", "am5", "lga4677", "bga" (soldered)
  bios_type: string?            # "uefi", "legacy", "coreboot", "uboot"
  physical_ports: PhysicalPort[]  # every external port with location and internal routing
  internal_headers: InternalHeader[]?  # internal connectors (USB headers, fan, RGB, audio)
  expansion_slots: ExpansionSlot[]?    # PCIe, M.2, DIMM slots
  power_connectors: string[]?   # "24pin_atx", "8pin_eps", "4pin_eps"
  vrm: VrmSpec?                 # voltage regulator (determines CPU power delivery quality)

PhysicalPort:
  id: string                    # unique port identifier ("rear_usb_a_1", "front_usb_c_1",
                                # "rear_hdmi_1", "rear_eth_1", "rear_audio_line_out")
  connector: string             # physical connector type (see table below)
  position: PortPosition        # where on the device body
  internal_path: string         # what controller this port connects to internally
  controller_id: string?        # which USB/PCIe/SATA controller
  shared_bandwidth: string?     # what other ports share bandwidth with this one
  speed: string?                # actual speed class (may differ from connector capability)
  color: string?                # physical port color (useful for user instructions)
  label: string?                # silkscreen label on the board/chassis
  power_delivery: PortPowerBudget?  # power available on this port (§2.10)

PortPosition:
  face: string                  # "rear", "front", "left", "right", "top", "bottom"
  row: uint?                    # row number (0 = top/leftmost)
  column: uint?                 # column number (0 = leftmost/topmost)
  offset_mm: Position2d?        # precise position on the face (mm from top-left of face)
  label_position: string?       # human-readable ("upper left", "lower right", "beside HDMI")
```

**Connector types**:

| Connector | Media types | Notes |
|-----------|------------|-------|
| `usb_a` | data, power | USB Type-A (2.0/3.0/3.1/3.2 — speed in `speed` field) |
| `usb_c` | data, power, video | USB Type-C (may carry USB, Thunderbolt, DP Alt, PD) |
| `usb_micro_b` | data, power | USB Micro-B (OTG on SBCs) |
| `hdmi` | video, audio | HDMI (1.4/2.0/2.1 — version in `speed` field) |
| `dp` | video, audio | DisplayPort (1.2/1.4/2.0/2.1) |
| `mini_dp` | video, audio | Mini DisplayPort |
| `usb_c_dp_alt` | video, audio, data, power | USB-C carrying DP Alt Mode |
| `usb_c_thunderbolt` | video, audio, data, power | USB-C carrying Thunderbolt |
| `rj45` | data | Ethernet (100M/1G/2.5G/5G/10G) |
| `3.5mm_audio` | audio | TRS/TRRS audio jack (line out, mic in, combo) |
| `optical_audio` | audio | TOSLINK S/PDIF optical |
| `coax_audio` | audio | Coaxial S/PDIF |
| `xlr` | audio | XLR (mic, AES3, DMX) |
| `6.35mm_audio` | audio | 1/4" TRS (instruments, headphones) |
| `bnc` | video, data, audio | BNC (SDI video, word clock, MADI) |
| `dvi` | video | DVI-I / DVI-D |
| `vga` | video | VGA (D-Sub 15) |
| `sma` | radio | SMA antenna connector (WiFi, LoRa, cellular) |
| `barrel_dc` | power | DC barrel jack (various sizes) |
| `gpio_header` | data, power | Pin header (SBC GPIO, internal USB header) |
| `m2` | data | M.2 slot (NVMe, WiFi, cellular) |
| `sata_data` | data | SATA data connector |
| `sata_power` | power | SATA power connector |
| `sd_card` | data | SD/microSD card slot |
| `sim` | data | SIM card slot (nano/micro/mini) |

**Internal header** (motherboard internal connectors):

```yaml
InternalHeader:
  id: string                    # "usb_header_1", "front_panel_audio", "rgb_12v_1"
  type: string                  # "usb2_header", "usb3_header", "usb_c_header",
                                # "front_panel_audio", "spdif_out",
                                # "argb_3pin", "rgb_12v_4pin", "fan_4pin", "fan_3pin",
                                # "front_panel_io", "tpm", "serial_com"
  position: PortPosition        # location on the board
  controller_id: string?        # which internal controller
  ports_provided: uint?         # how many ports this header provides (USB header = 2)
  shared_bandwidth: string?     # bandwidth sharing group
```

**Expansion slot**:

```yaml
ExpansionSlot:
  id: string                    # "pcie_x16_1", "m2_1", "dimm_a1"
  type: string                  # "pcie_x16", "pcie_x4", "pcie_x1",
                                # "m2_m_key_2280", "m2_e_key_2230",
                                # "dimm_ddr5", "dimm_ddr4"
  source: string                # "cpu" or "chipset" — where the lanes come from
  lanes: uint?                  # electrical lanes (may be fewer than physical slot)
  generation: uint?             # PCIe generation (3, 4, 5)
  shared_with: string[]?        # other slots/ports that share lanes with this one
  position: PortPosition        # physical location on the board
  populated: string?            # device database ID of installed device (if known)
```

**Example — a typical mini-ITX motherboard's port mapping**:

```yaml
id: "asrock-b760i-lightning-wifi"
type: motherboard
motherboard:
  form_factor: "mitx"
  chipset_id: "intel-b760"
  cpu_socket: "lga1700"

  physical_ports:
    # Rear I/O — left to right, top to bottom
    - id: rear_usb_a_1
      connector: usb_a
      position: { face: rear, row: 0, column: 0, label_position: "top left" }
      internal_path: "chipset → usb3.0_hub"
      speed: "usb3_5gbps"
      color: "blue"

    - id: rear_usb_a_2
      connector: usb_a
      position: { face: rear, row: 0, column: 1, label_position: "top left, second" }
      internal_path: "chipset → usb3.0_hub"
      speed: "usb3_5gbps"
      color: "blue"

    - id: rear_usb_c_1
      connector: usb_c
      position: { face: rear, row: 0, column: 2, label_position: "top centre" }
      internal_path: "cpu → usb3.2_gen2"
      speed: "usb3_10gbps"
      # NOTE: this port is on the CPU's native USB controller — faster than the
      # chipset ports. The router knows to recommend this port for high-bandwidth
      # devices (capture cards, fast storage).

    - id: rear_usb_a_3
      connector: usb_a
      position: { face: rear, row: 1, column: 0, label_position: "bottom left" }
      internal_path: "chipset → usb2.0_hub"
      speed: "usb2"
      color: "black"
      # NOTE: USB 2.0 only! On the rear I/O next to USB 3.0 ports. Easy to
      # plug a USB 3.0 device into the wrong port. The router detects this
      # and warns: "Capture card on rear_usb_a_3 is running at USB 2.0 speed.
      # Move it to rear_usb_c_1 for USB 3.2 Gen 2 (10 Gbps)."

    - id: rear_hdmi_1
      connector: hdmi
      position: { face: rear, row: 1, column: 3, label_position: "bottom right" }
      internal_path: "cpu → igpu → hdmi"
      speed: "hdmi_2.0"

    - id: rear_dp_1
      connector: dp
      position: { face: rear, row: 1, column: 4 }
      internal_path: "cpu → igpu → dp"
      speed: "dp_1.4"

    - id: rear_eth_1
      connector: rj45
      position: { face: rear, row: 1, column: 5 }
      internal_path: "chipset → i226v"
      speed: "2.5gbe"

  expansion_slots:
    - id: pcie_x16_1
      type: pcie_x16
      source: "cpu"
      lanes: 16
      generation: 4
      position: { face: top, row: 0 }

    - id: m2_1
      type: m2_m_key_2280
      source: "cpu"
      lanes: 4
      generation: 4
      shared_with: []            # direct to CPU, not shared
      position: { face: top, row: 1, label_position: "under heatsink" }

    - id: m2_2
      type: m2_m_key_2280
      source: "chipset"
      lanes: 4
      generation: 3              # chipset provides Gen 3, not Gen 4
      shared_with: ["sata_1", "sata_2"]  # shares lanes with SATA ports!
      position: { face: bottom }

  internal_headers:
    - id: usb_header_1
      type: usb3_header
      controller_id: "chipset_usb3"
      ports_provided: 2
      position: { face: top, label_position: "bottom edge near front panel" }

    - id: front_panel_usb_c
      type: usb_c_header
      controller_id: "cpu_usb32"
      ports_provided: 1
      position: { face: top, label_position: "bottom edge" }

    - id: argb_1
      type: argb_3pin
      position: { face: top, label_position: "top edge" }
```

This data enables the router to tell the user: "Your capture card is on
`rear_usb_a_3` which is USB 2.0 via the chipset. Move it to `rear_usb_c_1`
which is USB 3.2 Gen 2 direct from the CPU — you'll get 10× the bandwidth."
Without the physical port mapping, the router knows the device is on USB 2.0
but can't tell the user *which port to move it to*.

**CpuSpec**:

```yaml
CpuSpec:
  vendor: string?               # "intel", "amd", "apple", "qualcomm", "broadcom",
                                # "allwinner", "rockchip", "sophgo"
  family: string?               # "core_ultra", "ryzen", "m3", "bcm2712", "rk3588"
  model: string?                # "i5-13600K", "R7 7800X3D", "M3 Pro"
  cores: uint?                  # physical cores
  threads: uint?                # logical threads
  base_clock_mhz: uint?
  boost_clock_mhz: uint?
  tdp_w: float?                 # thermal design power
  igpu: IgpuSpec?               # integrated graphics (if present)
  memory_controller: MemoryControllerSpec?
  pcie_lanes: PcieLaneAllocation?  # how CPU PCIe lanes are allocated
  usb_controller: UsbControllerSpec?  # CPU-native USB controller (if any)
  npu: NpuSpec?                 # neural processing unit (if present)
  cache: CacheSpec?

IgpuSpec:
  name: string?                 # "Intel UHD 770", "Radeon 780M", "Apple GPU"
  encode: GpuCodecCapability?   # hardware encoder (Quick Sync, VCN)
  decode: GpuCodecCapability?   # hardware decoder
  display_outputs: uint?        # maximum simultaneous displays
  max_resolution: Resolution?

NpuSpec:
  name: string?                 # "Intel NPU", "Rockchip NPU", "Apple Neural Engine"
  tops: float?                  # tera operations per second
  supported_frameworks: string[]?  # "openvino", "rknn", "coreml", "onnx"

MemoryControllerSpec:
  type: string?                 # "ddr4", "ddr5", "lpddr5", "lpddr4x"
  channels: uint?               # memory channels
  max_speed_mhz: uint?          # maximum memory clock
  max_capacity_gb: uint?

PcieLaneAllocation:
  total_lanes: uint             # total CPU PCIe lanes
  allocations: LaneAllocation[]

LaneAllocation:
  destination: string           # "x16_slot", "m2_1", "chipset_uplink", "thunderbolt"
  lanes: uint
  generation: uint              # PCIe generation
  bifurcatable: bool?           # can be split (x16 → x8+x8, x8 → x4+x4)

UsbControllerSpec:
  ports: uint?                  # number of USB ports directly on the CPU
  speed: string?                # "usb3_10gbps", "usb3_20gbps", "usb4"
  # CPU USB ports are typically faster than chipset USB ports because they
  # don't traverse the chipset uplink. This matters for capture cards.
```

**ChipsetSpec** (PCH / southbridge — the hub that connects everything):

```yaml
ChipsetSpec:
  vendor: string?               # "intel", "amd"
  name: string?                 # "B760", "X670E", "Z790"
  uplink: ChipsetUplink         # connection to CPU
  usb_controllers: UsbControllerBlock[]
  sata_ports: uint?
  pcie_lanes: PcieLaneAllocation?
  other_features: string[]?     # "thunderbolt_4", "wifi_cnvi", "2.5gbe"

ChipsetUplink:
  type: string                  # "dmi_4.0" (Intel), "pcie_gen4" (AMD)
  bandwidth_gbps: float         # e.g., DMI 4.0 = ~16 GB/s (x8 Gen 4)
  # CRITICAL: this is the single bottleneck between CPU and everything
  # on the chipset. All chipset USB, SATA, PCIe, and Ethernet share this.

UsbControllerBlock:
  controller_id: string         # "usb3_hub_1", "usb2_hub_1"
  speed: string                 # "usb2", "usb3_5gbps", "usb3_10gbps"
  ports: uint                   # ports on this controller
  shared_bandwidth: bool        # do all ports share the controller's bandwidth?
  total_bandwidth_bps: uint64?  # total bandwidth across all ports on this controller
  # A USB 3.0 hub with 4 ports shares 5 Gbps among all 4. Two capture cards
  # on the same hub get 2.5 Gbps each, not 5 Gbps each.
```

**Why this matters for routing**: The router traces the full path from a
device to the CPU. A capture card on a chipset USB 3.0 port traverses:
USB device → USB 3.0 hub (shared 5 Gbps) → chipset → DMI uplink (shared
with all other chipset devices) → CPU. If the Ethernet NIC is also on the
chipset, heavy network traffic reduces available bandwidth for USB. The
router knows this from the internal topology and accounts for it in
bandwidth calculations.

On AMD platforms, the chipset uplink is typically 4× Gen 4 PCIe lanes
(~8 GB/s). On Intel, it's DMI 4.0 (~16 GB/s for x8). These numbers
determine the maximum aggregate throughput of all chipset-connected
devices — a real constraint that block diagrams rarely make clear.

**PhysicalPortInfo** (on the Port primitive in §2.2 — where this port is on
the device's body):

```yaml
PhysicalPortInfo:
  connector: string             # connector type from the table above
  position: PortPosition        # face, row, column, offset
  color: string?                # physical color of the port/surround
  label: string?                # printed label (silkscreen, chassis label)
  panel: string?                # which panel ("rear_io", "front_io", "side", "top")
  adjacent_to: string[]?        # IDs of physically adjacent ports (for user instructions)
  internal_path_summary: string? # human-readable path ("CPU → USB 3.2 Gen 2, direct")
```

This field on Port (§2.2) is populated from the device database when the
device is matched. The router uses it to generate user-facing recommendations:

- "Move your capture card from the black USB-A port (rear, lower left —
  USB 2.0) to the USB-C port (rear, top centre — USB 3.2 Gen 2 direct
  from CPU)"
- "Your capture card and external SSD are both on the same USB 3.0
  controller. Total bandwidth is shared at 5 Gbps. Consider moving
  one to the CPU-direct USB-C port."
- "M.2 slot 2 shares PCIe lanes with SATA ports 1–2. Your NVMe drive
  in M.2_2 disabled your SATA ports."

**InternalTopology** (the key to understanding compound devices):

```yaml
InternalTopology:
  sub_devices: SubDevice[]
  internal_links: InternalLink[]
  notes: string?                # free-text topology notes

SubDevice:
  id: string
  type: string                  # "usb_hub", "ethernet", "audio_codec", "dp_mst_hub"
  chipset: string?
  properties: PropertyBag?      # any relevant properties

InternalLink:
  from: string                  # sub-device or port ID
  to: string                    # sub-device or port ID
  bus: string                   # "usb2", "usb3", "pcie", "i2c", "thunderbolt"
  bandwidth_bps: uint64?        # link bandwidth
  shared: bool?                 # is this bandwidth shared with other internal links?
  notes: string?
```

### 15.5 3D models

Device entries can optionally reference a 3D model for use in the 3D desk
scene and for visual device recognition (matching captured images to known
devices):

```yaml
Model3dRef:
  format: string                # "glb", "gltf", "obj" (glTF Binary preferred)
  url: string                   # connect://device-db/models/<id>.glb or local path
  lod: Model3dLod[]?            # level-of-detail variants
  origin: string?               # where the model came from
  license: string?              # license for the model
  scale: float?                 # scale factor (1.0 = mm match dimensions_mm)
  recognition: RecognitionData? # feature data for visual matching

Model3dLod:
  level: uint                   # 0=highest detail, 1=medium, 2=low
  url: string
  triangle_count: uint?

RecognitionData:
  silhouette_hash: string?      # perceptual hash of device silhouette (for image matching)
  feature_descriptors: string?  # URL to pre-computed feature descriptors
  reference_images: string[]?   # URLs to reference photos from multiple angles
```

3D models are optional and never required. The parametric shape templates
(§ device-db.md) provide a fallback rendering for any device without a 3D
model. Models are stored on Connect and downloaded on demand.

### 15.6 Distribution via Connect

The device database is too large for every device to carry the entire thing.
Distribution is hierarchical:

```
Connect (canonical source)
    │
    ├── Full database (all entries, all models)
    │
    ├── Category packs (downloadable subsets)
    │   ├── keyboards (all keyboard entries)
    │   ├── monitors (all monitor entries)
    │   ├── capture-cards (all capture card entries)
    │   ├── thunderbolt-docks (all dock entries + topologies)
    │   ├── microphones (all mic entries + response curves)
    │   └── ... etc.
    │
    └── Per-device entries (individual downloads by VID/PID or ID)
```

**Controller behaviour**:

```yaml
DatabaseSyncPolicy:
  mode: string                  # "full", "detected", "manual"
  sync_interval_hours: uint     # how often to check for updates (default: 24)
  cache_size_mb: uint?          # max local cache size
  include_3d_models: bool       # download 3D models (large, optional)
  include_response_curves: bool # download audio response data
  distribute_via_mesh: bool     # push relevant entries to nodes

# mode descriptions:
# "full"     — download everything. For controllers with storage.
# "detected" — download entries matching detected hardware (VID/PID match).
#              Plus category packs for device types in the graph.
# "manual"   — only download entries explicitly requested by user.
```

**Mesh distribution**: When a controller downloads an entry, it can push
relevant entries to nodes via the mesh. A node doesn't need the full database —
it only needs entries for devices physically connected to it. The controller
knows what's connected (from USB enumeration) and pushes matching entries.

**Offline operation**: The database is cached locally. If Connect is unreachable,
the cached version is used. New devices without a cached entry get `assumed`
quality properties from their USB device class or mDNS TXT records.

### 15.7 Custom and private entries

Users can create custom entries for devices not in the public database:

```yaml
# Custom entry for a one-off USB device
id: custom-my-3d-printer-usb-hub
type: usb_hub
name: My 3D Printer's Internal USB Hub
vendor: Creality
model: Ender 3 V3 SE (internal hub)
hub:
  ports:
    - { id: "port1", speed: "usb2", type: "internal" }
    - { id: "port2", speed: "usb2", type: "internal" }
  upstream_speed: "usb2"
  internal_hub_speed: "usb2"
  chipset: "unknown"
usb: [{ vid: "0x1A86", pid: "0x7523" }]
confidence: "estimated"
sources:
  - { type: "user", contributor: "local", note: "Discovered by lsusb -t" }
```

Custom entries:
- Are stored locally on the controller
- Can optionally be submitted to Connect for public inclusion
- Override public entries when they have the same `id` (user customisation)
- Can be shared across a user's mesh via Connect sync

Private entries (devices a user doesn't want to share publicly) stay local
and are never uploaded to Connect.

### 15.8 Matching and auto-detection

When a new device appears in the graph (USB hotplug, mDNS discovery, Bluetooth
pairing), the controller tries to match it to a database entry:

1. **USB VID/PID match**: Exact match on `usb[].vid` and `usb[].pid`
2. **Bluetooth name/OUI match**: Pattern match on `bluetooth` identifier
3. **mDNS type match**: Match on `network.mdns_type`
4. **Fuzzy match**: If no exact match, search by vendor + model name from
   USB string descriptors or mDNS TXT records
5. **Category fallback**: If no specific match, use the device's USB class
   or mDNS role to assign a generic template entry

Match results:
- **Exact match**: Full entry applied, `reported` quality for database fields
- **Fuzzy match**: Entry applied with `assumed` quality, user prompted to confirm
- **No match**: Generic template applied with `assumed` quality. User can create
  a custom entry or request the community to add one.

### 15.9 Quality and provenance

Every entry and every field within an entry has provenance tracking:

```yaml
Source:
  type: string                  # "manufacturer", "measured", "openrgb", "community",
                                # "user", "teardown", "datasheet", "review"
  contributor: string?          # who (GitHub handle, "local", manufacturer name)
  url: string?                  # source URL (datasheet, review, OpenRGB commit)
  date: string?                 # ISO date
  method: string?               # how it was obtained ("calipers", "photo_analysis",
                                # "usb_trace", "lsusb", "teardown_photo", "spec_sheet")
  note: string?                 # free-text context
```

The `confidence` field on the entry reflects overall data quality:

| Confidence | Meaning |
|-----------|---------|
| `exact` | From manufacturer CAD, verified measurement, or official datasheet |
| `approximate` | From photos, reviews, or community measurement (±2mm physical, ±5% electrical) |
| `estimated` | Rough data, may be wrong. Better than nothing. |
| `community` | Community-contributed, unverified. Trusted but not guaranteed. |
| `template` | Inherited from a generic template. No device-specific data. |

Contributions that improve confidence (e.g., replacing `estimated` dimensions
with `exact` measurements from calipers) are always welcome and surface in the
contribution workflow.

### 15.10 Relationship to device-db.md

The existing `device-db.md` specification (in this repo) defines the detailed
schema for spatial RGB layout, 3D world positioning, shape templates, LED
placement, case grouping, monitor libraries, and furniture positioning. That
specification is not replaced by this section — it is the detailed
implementation of the `rgb`, `shape_template`, and world layout aspects of the
device database.

This section (§15) defines the **universal entry schema** and the
**distribution/matching/provenance** model that wraps around it. The RGB-specific
fields (`leds[]`, `zones[]`, `key_rows[]`, `led_path`, etc.) from device-db.md
live within the `rgb` block of the universal entry schema.

### 15.11 API

```
GET /api/v1/device-db/search?q=keychron    # search entries by name/vendor/tag
GET /api/v1/device-db/match?vid=0x3434&pid=0x0930  # match by USB VID/PID
GET /api/v1/device-db/entry/{id}           # get full entry
PUT /api/v1/device-db/entry/{id}           # create/update custom entry
DELETE /api/v1/device-db/entry/{id}        # delete custom entry (public entries can't be deleted)
POST /api/v1/device-db/submit/{id}         # submit custom entry for public inclusion
GET /api/v1/device-db/categories           # list available category packs
POST /api/v1/device-db/sync               # trigger sync from Connect
GET /api/v1/device-db/stats               # local cache stats (entries, size, last sync)
```

---

## 16. Node Definition

A node is the fundamental building block of the Ozma mesh. This section
defines how a node is described entirely in terms of the routing spec — its
hardware composition, USB gadget presentation, physical I/O, lifecycle, and
relationship to the target machine it serves.

### 16.1 Node as a composite device

A node is a **compound device** in the routing graph. It is composed of:

```yaml
NodeDefinition:
  id: string                    # node identity (from enrollment)
  name: string                  # human-readable name
  role: string                  # "compute", "video", "audio", "room-mic", "display", "sensor"
  platform: PlatformSpec        # what hardware this node runs on
  target_binding: TargetBinding # how this node connects to its target machine
  gadget: GadgetSpec            # USB gadget composite device presented to the target
  peripherals: PeripheralBus[]  # physical I/O buses and what's connected
  services: NodeService[]       # software services running on this node
  network: NodeNetwork          # network configuration and mesh membership
  identity: NodeIdentity        # cryptographic identity and enrollment state
```

### 16.2 Platform

The hardware platform the node runs on:

```yaml
PlatformSpec:
  hardware: string              # device database entry ID (e.g., "milkv-duo-s", "rpi-zero2w")
  soc: string?                  # SoC identifier ("sg2000", "bcm2710", "allwinner-h616")
  arch: string                  # "riscv64", "aarch64", "armv7l", "x86_64"
  cpu_cores: uint
  cpu_freq_mhz: uint?
  memory_mb: uint
  storage_mb: uint?             # eMMC/SD card size
  usb_otg: bool                 # has USB OTG port (required for gadget mode)
  usb_host_ports: uint?         # number of USB host ports
  gpio_pins: uint?              # number of GPIO pins
  i2c_buses: uint?              # number of I2C buses
  spi_buses: uint?              # number of SPI buses
  hardware_codecs: string[]?    # available HW codecs ("h264_v4l2m2m", "jpeg_hw")
  ethernet: bool?
  wifi: string?                 # "wifi4", "wifi5", "wifi6"
  bluetooth: string?            # "5.0", "5.3"
  poe: bool?                    # Power over Ethernet capable
  power_draw_w: float?          # typical power consumption
```

### 16.3 Target binding

The fundamental invariant: a node is **permanently wired** to one target
machine. This relationship is expressed in the graph:

```yaml
TargetBinding:
  target_id: string?            # the target machine this node serves (null if unbound)
  connection_type: string       # "usb_cable", "usb_c", "internal_header", "virtual"
  usb_port: string?             # which USB port on the target ("front_top", "rear_3", etc.)
  cable_length_m: float?        # physical cable length (affects latency/signal quality)
  power_source: string?         # "target_usb", "external_5v", "poe", "usb_pd"
  power_budget_ma: uint?        # available current from the target USB port

  # What the target sees:
  gadget_ref: string            # references the GadgetSpec defining what's presented

  # Lights-out management (LoM):
  lom: LomSpec?                 # physical power/reset control over the target
```

**LomSpec** — physical control over the target machine's power state:

```yaml
LomSpec:
  power_button: GpioPin?       # wired to target's front panel power header
  reset_button: GpioPin?       # wired to target's front panel reset header
  power_led: GpioPin?          # reads target's power LED state
  hdd_led: GpioPin?            # reads target's HDD LED state
  wake_on_lan: bool             # target supports WoL (MAC address known)
  wake_on_lan_mac: string?      # target's MAC for WoL magic packet
  bmc: string?                  # if target has a BMC ("ipmi", "ilo", "idrac", "amt")
  bmc_address: string?          # BMC network address

GpioPin:
  pin: uint                     # GPIO pin number
  active_low: bool              # true if signal is active low
  mode: string                  # "output" (relay), "input" (sense), "open_drain"
  hold_ms: uint?                # for buttons: how long to hold (power: 200ms, force-off: 5000ms)
```

### 16.4 USB gadget specification

The gadget spec defines the composite USB device the node presents to the
target machine. This is what the target's OS sees when it enumerates the
node's USB port.

```yaml
GadgetSpec:
  name: string                  # gadget name in ConfigFS
  vendor_id: string             # USB VID (e.g., "0x1d6b")
  product_id: string            # USB PID (e.g., "0x0104")
  device_class: uint?           # USB device class (0 = composite)
  manufacturer: string          # USB manufacturer string
  product: string               # USB product string
  serial_number: string         # USB serial number (unique per node)
  usb_version: string           # "2.0", "3.0"
  max_power_ma: uint            # max current draw declared to host
  functions: GadgetFunction[]   # USB functions in the composite device

GadgetFunction:
  type: string                  # function type (see table below)
  name: string                  # ConfigFS function name (e.g., "hid.keyboard")
  enabled: bool                 # is this function active?
  config: GadgetFunctionConfig  # type-specific configuration
  port: PortRef                 # the routing graph port this function creates
```

**Gadget function types**:

| Type | ConfigFS function | Creates port | What the target sees |
|------|------------------|-------------|---------------------|
| `hid_keyboard` | `hid.keyboard` | HID sink | Boot protocol keyboard |
| `hid_mouse` | `hid.mouse` | HID sink | Absolute pointer (digitizer) |
| `hid_gamepad` | `hid.gamepad` | HID sink | Gamepad/joystick |
| `hid_consumer` | `hid.consumer` | HID sink | Consumer control (media keys) |
| `uac2_speaker` | `uac2.speaker` | Audio source | USB speaker (target sends audio to node) |
| `uac2_mic` | `uac2.mic` | Audio sink | USB microphone (node sends audio to target) |
| `uvc_camera` | `uvc.camera` | Video sink | USB webcam (node sends video to target) |
| `mass_storage` | `mass_storage.0` | Data port | USB mass storage (flash drive) |
| `ecm_ethernet` | `ecm.usb0` | Network port | USB ethernet adapter (network to target) |
| `serial` | `acm.serial` | Data port | USB serial port |
| `vendor` | `vendor.0` | Data port | Vendor-specific (cloned device descriptors) |

**HID function configuration**:

```yaml
HidFunctionConfig:
  protocol: uint                # 0=none, 1=keyboard, 2=mouse
  subclass: uint                # 0=none, 1=boot interface
  report_length: uint           # report size in bytes
  report_descriptor: bytes      # HID report descriptor (binary)
  # OR:
  report_descriptor_template: string  # named template ("boot_keyboard_8byte",
                                      # "absolute_pointer_6byte", "gamepad_12byte")
```

**UAC2 function configuration**:

```yaml
Uac2FunctionConfig:
  direction: string             # "playback" (target→node), "capture" (node→target)
  sample_rate: uint             # Hz
  channels: uint                # 1=mono, 2=stereo
  bit_depth: uint               # 16, 24, 32
  channel_mask: uint?           # channel position mask
```

**UVC function configuration**:

```yaml
UvcFunctionConfig:
  resolutions: Resolution[]     # supported resolutions
  framerates: float[]           # supported framerates
  format: string                # "mjpeg", "yuyv", "nv12"
  max_payload_size: uint?       # max frame size in bytes
```

**Device emulation** (§ usb_emulation.py):

```yaml
VendorFunctionConfig:
  mode: string                  # "clone", "passthrough", "emulate"
  profile: string?              # device database entry ID or USB profile ID
  source_device: DeviceRef?     # for clone/passthrough: the real device to clone from
  descriptor_tree: UsbDescriptorTree?  # full USB descriptor tree (from profile or captured)
  intercept_endpoints: EndpointInterceptRule[]?  # which endpoints to intercept vs forward
```

The gadget spec lets you define exactly what the target machine sees. A
minimal node presents just HID keyboard + mouse. A full node adds UAC2
audio, UVC camera, and possibly a cloned vendor device for RGB passthrough.

### 16.5 Physical I/O buses

Nodes have physical I/O beyond USB. These connect sensors, actuators, LEDs,
and screens:

```yaml
PeripheralBus:
  type: string                  # "gpio", "i2c", "spi", "uart", "pwm", "onewire"
  bus_id: string                # bus identifier ("i2c-1", "spidev0.0", "gpio")
  devices: BusDevice[]          # what's connected to this bus

BusDevice:
  type: string                  # device type from §2.1 or §2.9
  device_db_id: string?         # device database entry
  address: string?              # bus address (I2C: "0x40", SPI: "CS0", GPIO: "pin17")
  driver: string?               # driver or plugin ID
  port_mappings: PortMapping[]  # maps bus device to routing graph ports

PortMapping:
  bus_endpoint: string          # endpoint on the bus device
  graph_port: PortRef           # port in the routing graph
```

**Example — a complete hardware node's peripheral bus configuration**:

```yaml
peripherals:
  - type: gpio
    bus_id: gpio
    devices:
      - type: actuator           # power relay
        address: "pin17"
        driver: "gpio_relay"
        port_mappings:
          - { bus_endpoint: "relay_out", graph_port: "lom.power_button" }
      - type: actuator           # reset relay
        address: "pin27"
        driver: "gpio_relay"
        port_mappings:
          - { bus_endpoint: "relay_out", graph_port: "lom.reset_button" }
      - type: sensor             # power LED sense
        address: "pin22"
        driver: "gpio_input"
        port_mappings:
          - { bus_endpoint: "digital_in", graph_port: "lom.power_led_sense" }
      - type: rgb                 # WS2812 addressable LEDs
        address: "pin18"          # data pin
        driver: "ws2812_spi"
        device_db_id: "generic-ws2812b-strip"
        port_mappings:
          - { bus_endpoint: "led_data", graph_port: "rgb.strip_out" }

  - type: i2c
    bus_id: "i2c-1"
    devices:
      - type: sensor             # current sensor
        address: "0x40"
        driver: "ina219"
        device_db_id: "ina219"
        port_mappings:
          - { bus_endpoint: "measurement", graph_port: "sensor.current" }
      - type: sensor             # temperature/humidity
        address: "0x76"
        driver: "bme280"
        device_db_id: "bme280"
        port_mappings:
          - { bus_endpoint: "measurement", graph_port: "sensor.environment" }
      - type: screen             # status OLED
        address: "0x3c"
        driver: "ssd1306"
        device_db_id: "ssd1306-128x64"
        port_mappings:
          - { bus_endpoint: "display", graph_port: "screen.status" }
```

### 16.6 Node services

Software services running on the node that create ports in the routing graph:

```yaml
NodeService:
  id: string                    # service identifier
  type: string                  # service type
  enabled: bool
  config: ServiceConfig         # type-specific configuration
  ports: PortRef[]              # ports this service creates in the graph
  resource_cost: ResourceCost   # resources this service consumes (§2.7)
```

**Standard node services**:

| Service | Type | Ports created | Description |
|---------|------|--------------|-------------|
| HID receiver | `hid_udp_receiver` | HID sink (UDP) | Receives HID from controller |
| Video capture | `video_capture` | Video source | V4L2 capture + encode |
| Audio bridge | `audio_bridge` | Audio source + sink | UAC2 ↔ VBAN/Opus |
| RGB controller | `rgb_controller` | RGB sink | Drives addressable LEDs |
| mDNS announcer | `mdns` | — | Service discovery |
| Binary channel | `binary_channel` | Control port | Spec 09 event/command |
| HTTP server | `http_server` | — | Serves streams and API |
| Self-management | `self_management` | Data source | CPU/temp/memory metrics |
| Connect client | `connect_client` | — | Registration, relay, heartbeat |
| RPA engine | `node_rpa` | Control port | OCR + BIOS navigation |
| Serial capture | `serial_capture` | Data source | Serial console output |
| Phone endpoint | `phone_endpoint` | Audio source + sink | Phone UAC2 bridge |

### 16.7 Network and mesh membership

```yaml
NodeNetwork:
  interfaces: NetworkInterface[]
  mesh: MeshMembership
  mdns: MdnsConfig

NetworkInterface:
  name: string                  # "eth0", "wlan0", "wg0"
  type: string                  # "ethernet", "wifi", "wireguard"
  address: string?              # IP address
  mac: string?                  # MAC address

MeshMembership:
  overlay_ip: string            # WireGuard overlay address (10.200.x.x)
  controller_id: string?        # enrolled controller's ID
  enrollment_state: string      # "unenrolled", "pending", "enrolled", "revoked"
  public_key: string?           # WireGuard public key
  connect_registered: bool      # registered with Ozma Connect

MdnsConfig:
  instance_name: string         # e.g., "ozma-node-a3f2._ozma._udp.local"
  txt_records:                  # advertised TXT records
    proto: string               # protocol version ("ozma/0.1")
    role: string                # node role ("compute", "video", etc.)
    caps: string                # comma-separated capabilities ("hid,video,audio")
    hw: string                  # hardware platform ("milkv-duos", "rpi4")
    fw: string                  # firmware version
    hid_port: uint              # HID listener port
    audio_port: uint?           # VBAN audio port
    stream_port: uint?          # HTTP stream port
```

### 16.8 Node identity and enrollment

```yaml
NodeIdentity:
  node_id: string               # globally unique identifier
  identity_key: Ed25519Key      # node's identity keypair
  enrollment: EnrollmentState
  certificates: Certificate[]?  # issued by controller's mesh CA

EnrollmentState:
  state: string                 # "factory_new", "enrolled", "orphaned", "revoked"
  controller_id: string?        # which controller this node is enrolled with
  enrolled_at: timestamp?
  enrollment_method: string?    # "mdns_auto", "manual_register", "connect_relay"
```

### 16.9 Node lifecycle

A node progresses through a defined lifecycle:

```
Power on
  → Hardware init (GPIO, I2C, SPI setup)
  → USB gadget creation (ConfigFS — present HID/UAC2/UVC to target)
  → Network init (DHCP, WireGuard tunnel)
  → Service startup (HID receiver, video capture, audio bridge, etc.)
  → Discovery (mDNS announcement OR direct registration with controller)
  → Enrollment (key exchange with controller, join mesh)
  → Operational (ready to receive HID, stream video, etc.)
  → Graph participation (controller adds node's ports to routing graph)
```

Each stage maps to spec primitives:

| Lifecycle stage | Spec primitive |
|----------------|---------------|
| Hardware init | PeripheralBus setup (§16.5) |
| USB gadget | GadgetSpec activation (§16.4), creates ports |
| Network init | NetworkInterface, transport plugin setup |
| Service startup | NodeService activation (§16.6), creates ports |
| Discovery | Topology discovery layer 3 (§9) |
| Enrollment | NodeIdentity (§16.8), security model (§10) |
| Operational | All ports active, pipelines can be assembled |
| Graph participation | Device + ports appear in controller's routing graph |

### 16.10 Worked example — defining a hardware node

A Milk-V Duo S node with capture card, LoM relays, RGB strip, and INA219
current sensor, described entirely in spec terms:

```yaml
id: "node-desk-01"
name: "Desk Node 1"
role: "compute"

platform:
  hardware: "milkv-duo-s"       # device database entry
  soc: "sg2000"
  arch: "riscv64"
  cpu_cores: 1
  cpu_freq_mhz: 1000
  memory_mb: 512
  storage_mb: 256               # microSD
  usb_otg: true
  gpio_pins: 26
  i2c_buses: 2

target_binding:
  target_id: "gaming-pc"
  connection_type: "usb_c"
  cable_length_m: 0.5
  power_source: "target_usb"
  power_budget_ma: 500
  gadget_ref: "gadget-standard"
  lom:
    power_button: { pin: 17, active_low: false, mode: "output", hold_ms: 200 }
    reset_button: { pin: 27, active_low: false, mode: "output", hold_ms: 200 }
    power_led: { pin: 22, active_low: false, mode: "input" }
    wake_on_lan: true
    wake_on_lan_mac: "AA:BB:CC:DD:EE:FF"

gadget:
  name: "ozma"
  vendor_id: "0x1d6b"
  product_id: "0x0104"
  manufacturer: "OzmaLabs"
  product: "Ozma Node"
  serial_number: "OZMA-DESK01"
  usb_version: "2.0"
  max_power_ma: 100
  functions:
    - type: hid_keyboard
      name: "hid.keyboard"
      enabled: true
      config:
        report_descriptor_template: "boot_keyboard_8byte"
      port: "hid.kbd_out"
    - type: hid_mouse
      name: "hid.mouse"
      enabled: true
      config:
        report_descriptor_template: "absolute_pointer_6byte"
      port: "hid.mouse_out"
    - type: uac2_speaker
      name: "uac2.speaker"
      enabled: true
      config: { direction: "playback", sample_rate: 48000, channels: 2, bit_depth: 16 }
      port: "audio.from_target"
    - type: uac2_mic
      name: "uac2.mic"
      enabled: true
      config: { direction: "capture", sample_rate: 48000, channels: 2, bit_depth: 16 }
      port: "audio.to_target"

peripherals:
  - type: gpio
    bus_id: gpio
    devices:
      - { type: rgb, address: "pin18", driver: "ws2812_spi",
          device_db_id: "generic-ws2812b-30led",
          port_mappings: [{ bus_endpoint: "led_data", graph_port: "rgb.strip" }] }
  - type: i2c
    bus_id: "i2c-1"
    devices:
      - { type: sensor, address: "0x40", driver: "ina219",
          port_mappings: [{ bus_endpoint: "measurement", graph_port: "sensor.current" }] }

services:
  - { id: "hid_rx", type: "hid_udp_receiver", enabled: true,
      ports: ["net.hid_in"], resource_cost: { cpu_percent: 1 } }
  - { id: "capture", type: "video_capture", enabled: true,
      config: { device: "/dev/video0", encoder: "mjpeg" },
      ports: ["video.capture_out"], resource_cost: { cpu_percent: 30, memory_mb: 150 } }
  - { id: "audio", type: "audio_bridge", enabled: true,
      ports: ["audio.vban_out", "audio.vban_in"],
      resource_cost: { cpu_percent: 2 } }

network:
  interfaces:
    - { name: "eth0", type: "ethernet", address: "10.0.100.12" }
    - { name: "wg0", type: "wireguard", address: "10.200.0.12" }
  mesh:
    overlay_ip: "10.200.0.12"
    controller_id: "ctrl-main"
    enrollment_state: "enrolled"
  mdns:
    instance_name: "ozma-node-desk01._ozma._udp.local"
    txt_records:
      proto: "ozma/0.1"
      role: "compute"
      caps: "hid,video,audio,rgb,lom,sensors"
      hw: "milkv-duos"
      fw: "0.5.1"
      hid_port: 7331
      audio_port: 6980
      stream_port: 7382

# The routing graph for this node has these ports:
# Sources: video.capture_out, audio.from_target, sensor.current, lom.power_led_sense
# Sinks:   hid.kbd_out, hid.mouse_out, audio.to_target, rgb.strip, net.hid_in
# Bidirectional: audio.vban_out, audio.vban_in (network audio)
```

This definition is complete — everything about the node's hardware, what it
presents to the target, its physical I/O, its services, its network identity,
and its ports in the routing graph is expressed in spec terms. A different
node (e.g., a Raspberry Pi Zero 2 W with no capture card) would have a
different `platform`, a simpler `gadget` (just HID), no `video_capture`
service, and fewer peripherals — but the same schema.

---

## Appendix A: Worked Example

### Scenario: User switches from "Desktop" to "Gaming"

**Setup**: Controller with two hardware nodes (Node A → Workstation, Node B →
Gaming PC). Both on the same LAN. Node B has a capture card (USB 3.0, supports
1080p60 MJPEG). Node B is connected to a GPU with NVENC.

**Graph** (relevant subset):

```
Controller
  ├── Port: kbd-capture (HID source, evdev)
  ├── Port: mouse-capture (HID source, evdev)
  ├── Port: audio-out (audio sink, PipeWire)
  └── Port: udp-7331 (HID sink, network)

Node B (LAN: 10.0.100.12, overlay: 10.200.0.12)
  ├── Port: udp-7331 (HID sink, network)
  ├── Port: hidg0 (HID sink, USB gadget → Gaming PC)
  ├── Port: v4l2-cap (video source, capture card output)
  ├── Port: vban-6980 (audio source, network)
  └── Port: usb-audio (audio source, UAC2 gadget ← Gaming PC)

Gaming PC (target)
  ├── Port: hdmi-out (video source → capture card)
  ├── Port: usb-hid (HID sink, sees Node B as keyboard/mouse)
  └── Port: usb-audio (audio source, sees Node B as speaker)
```

**Intent**: `gaming`

**Router computation**:

1. **HID pipeline**: Controller:kbd-capture → udp-aead → Node B:udp-7331 →
   internal → Node B:hidg0 → usb-gadget → Gaming PC:usb-hid
   - Latency: <1ms (evdev) + <1ms (LAN UDP) + <1ms (gadget write) = ~2ms ✓ (<5ms)
   - Format: boot protocol keyboard, 8 bytes, 1000 Hz

2. **Video pipeline**: Gaming PC:hdmi-out → hdmi-cable → Node B:capture-card:hdmi-in →
   internal → Node B:capture-card:usb-out → v4l2 → Node B:v4l2-cap →
   ffmpeg(MJPEG→H.264, NVENC) → udp-aead → Controller:video-sink
   - Latency: ~1ms (capture) + ~3ms (NVENC encode) + ~1ms (LAN UDP) = ~5ms ✓ (<16ms)
   - Format: 1080p60 H.264 high profile, ~15 Mbps
   - Note: MJPEG from capture card is transcoded to H.264 by NVENC (1 conversion)

3. **Audio pipeline**: Gaming PC:usb-audio → Node B:usb-audio → pipewire →
   Node B:vban-6980 → vban → Controller:audio-in → pipewire → Controller:audio-out
   - Latency: ~5ms (UAC2 buffer) + ~5ms (VBAN frame) + ~1ms (LAN) = ~11ms ✓ (<20ms)
   - Format: PCM 48000 Hz, stereo, 16-bit (VBAN native, no conversion)

**Result**: Three pipelines, all satisfying `gaming` intent constraints. Total
HID latency ~2ms, video ~5ms, audio ~11ms. Zero degradation needed.

**Switching**: When the user presses the switch hotkey, the controller:
1. Deactivates Node A's HID pipeline (stops sending UDP to Node A:7331)
2. Activates Node B's pre-computed HID pipeline (starts sending UDP to Node B:7331)
3. Activates Node B's video pipeline (starts ffmpeg capture + transcode + stream)
4. Activates Node B's audio pipeline (starts VBAN stream)

Steps 1–2 complete in <5ms (UDP redirect). Steps 3–4 complete in <100ms
(process startup). HID switching is near-instantaneous; video/audio follow
within one frame.

---

## Appendix B: Format Negotiation Worked Example

### Capture card format selection

**Port capabilities**:

```yaml
# Capture card V4L2 output port
capabilities:
  formats:
    - codec: mjpeg
      resolution: { min: {w: 640, h: 480}, max: {w: 1920, h: 1080} }
      framerate: { min: 1, max: 60 }
    - codec: raw
      resolution: { min: {w: 640, h: 480}, max: {w: 1920, h: 1080} }
      framerate: { min: 1, max: 30 }  # raw limited by USB bandwidth

# Network transport (udp-aead)
capabilities:
  formats:
    - codec: [h264, h265, mjpeg, av1]  # any compressed format
      resolution: { min: {w: 1, h: 1}, max: {w: 7680, h: 4320} }
      framerate: { min: 1, max: 240 }

# Hardware encoder (NVENC)
capabilities:
  formats:
    - input: { codec: [raw, mjpeg] }  # accepts raw or MJPEG
      output: { codec: [h264, h265], resolution: { max: {w: 4096, h: 4096} } }
```

**Intent**: `gaming` — wants ≤16ms latency, prefers hardware codec, prefers
lower latency over higher quality.

**Negotiation**:

1. **Enumerate**: Capture outputs MJPEG or raw. NVENC accepts both, outputs
   H.264 or H.265. Network accepts any compressed format.

2. **Restrict**: Intent forbids nothing. Intersection: MJPEG→H.264, MJPEG→H.265,
   raw→H.264, raw→H.265 are all viable paths through the encoder.

3. **Fixate**:
   - MJPEG input preferred over raw (less USB bandwidth, capture card hardware
     compresses it)
   - H.264 output preferred over H.265 (lower encode latency on NVENC, intent
     prefers lower latency)
   - Resolution: 1080p (max capture card supports)
   - Framerate: 60 (intent preference, capture card supports it with MJPEG)

   **Selected**: Capture(MJPEG 1080p60) → NVENC(MJPEG→H.264 1080p60) → UDP

---

## Appendix C: Intent Composition Example

### `control` + `fidelity_audio`

User is typing on a headless audio workstation — needs HID input and
uncompressed multichannel audio, no video.

**Composition**:

```yaml
# Merged from control + fidelity_audio
streams:
  - media_type: hid
    required: true                    # from control
    constraints:
      max_latency_ms: 5              # from control
      max_loss: 0.001                # from control
    preferences:
      prefer_lower_latency: true     # from control

  - media_type: audio
    required: true                    # from fidelity_audio
    constraints:
      max_latency_ms: 10             # from fidelity_audio
      forbidden_formats:
        - { lossy: true }            # from fidelity_audio
    preferences:
      target_channels: 8             # from fidelity_audio
      target_sample_rate: 96000      # from fidelity_audio
      target_bit_depth: 32           # from fidelity_audio
      prefer_lossless: true          # from fidelity_audio

  - media_type: video
    required: false                   # both say false

priority: 90                          # max(control:90, fidelity_audio:60)
```

The router builds two pipelines: one for HID (tiny, fast), one for audio
(high bandwidth, lossless). No video pipeline is created.
