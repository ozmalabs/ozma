# Graph Primitives

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in RFC 2119.

## Abstract

This document specifies the four graph primitives — Device, Port, Link, and
Pipeline — that compose the Ozma routing graph. Together they describe the
complete signal fabric from any source to any destination, including device
identity, port capabilities, link characteristics, and assembled pipeline
chains. Every routing decision operates on these primitives.

## Specification

The routing graph is composed of four primitives: **Device**, **Port**, **Link**,
and **Pipeline**. Together they describe the complete signal fabric from any
source to any destination.

### Device

A device is any physical or virtual thing that has ports. Devices form the nodes
of the routing graph.

Every device MUST have an `id`, `name`, `type`, and `location`. Every device
MUST have a `ports` array (which MAY be empty). The remaining fields are
OPTIONAL.

```yaml
Device:
  id: string                    # globally unique (derived from hardware identity or assigned)
  name: string                  # human-readable label
  type: enum                    # see Device Types below
  identity: HardwareIdentity?   # hardware serial numbers and unique identifiers
  device_db_id: string?         # matched device database entry (§15)
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
  freshness: DeviceFreshness?   # data age and refresh state per data class (see §5.2)
```

#### HardwareIdentity

Every device SHOULD carry a `HardwareIdentity` containing every discoverable
unique identifier for the device. Implementations MUST use the following
discovery priority when selecting the canonical serial: USB serial > SMBIOS
serial > MAC > PCI ID > user-assigned. If a device has no discoverable serial,
the user MAY assign one manually (e.g., reading the serial sticker on the back
of a monitor).

```yaml
HardwareIdentity:
  serial_number: string?        # primary serial number (from USB descriptor, SMBIOS,
                                # SMART, EDID, SPD, EEPROM, label, etc.)
  serial_source: string?        # where the serial came from ("usb", "smbios", "smart",
                                # "edid", "spd", "sfp_eeprom", "label", "user")
  uuid: string?                 # UUID (SMBIOS system UUID, disk UUID, etc.)
  mac_addresses: string[]?      # MAC addresses (one per network interface)
  usb_vid_pid: string?          # USB VID:PID ("1b1c:0150")
  usb_serial: string?           # USB serial number string descriptor
  pci_id: string?               # PCI vendor:device:subsystem ("8086:1533:15a1")
  sas_wwn: string?              # SAS World Wide Name
  wwn: string?                  # generic World Wide Name (FC, SAS)
  asset_tag: string?            # asset tag (from SMBIOS, or user-assigned)
  manufacturer_date: string?    # manufacturing date if available
  board_serial: string?         # motherboard serial (SMBIOS type 2)
  chassis_serial: string?       # chassis serial (SMBIOS type 3)
  system_serial: string?        # system serial (SMBIOS type 1)
  # Devices MAY have multiple identifiers from different sources.
  # The `id` field on Device is Ozma's stable identifier (MUST survive
  # reconnection). HardwareIdentity carries the raw hardware serials
  # for asset tracking, warranty lookup, and deduplication.
```

#### Device Types

The `type` field MUST be one of the following values:

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
| `ups` | UPS — battery backup, power conditioning, load monitoring |
| `pdu` | Power Distribution Unit — rack or desk, metered/switched/basic |
| `power_strip` | Power strip, surge protector, extension cord |
| `lock` | Smart lock, badge reader, access controller, intercom |
| `hvac` | Thermostat, CRAC, CRAH, split system, fan coil unit |
| `lighting` | Smart lighting controller, dimmer, scene controller (non-RGB — for room lighting) |
| `occupancy` | Occupancy/presence sensor, people counter, desk booking sensor |
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
| `avr` | AV receiver (HDMI switch + audio processor + amplifier + streaming) |
| `virtual` | Any software-defined device (loopback, pipe, virtual display, macro source) |

A device MAY be **compound** — a Thunderbolt dock contains a USB hub, an
Ethernet adapter, a display output, and an audio codec, each modelled as a
sub-device with their own ports, connected by internal links. The `topology`
field expresses this internal structure.

A device MAY be **switchable** — an external KVM switch or HDMI matrix has
multiple input ports and one or more output ports, with a configurable internal
routing matrix. See §2.5 for the switch model.

#### Location

Every entity in the graph MUST have a location, which has both a logical
component (bus topology, network address) and a physical component (where it is
in the real world). Physical location is OPTIONAL but enables spatial routing
(§8.1), zone inference, and the 3D scene.

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

#### PhysicalLocation

PhysicalLocation describes where something is in the real world. The `quality`
field MUST be present and MUST indicate how the position was determined.

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

#### RelativeLocation

Position MAY be defined relative to another entity. This is how most physical
locations are actually specified. A keyboard is not at absolute coordinates
(450, 300, 720) — it is "on the desk, centered".

A RelativeLocation MUST reference a `parent_id` and `parent_type`. The
`relationship` field MUST be one of the defined relationship types.

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
entities with `relative_to` pointing at that parent MUST move with it. This is
the same grouping model as case groups in device-db.md, but generalised to
any entity.

#### Location applicability

The `location` field exists on Device and is inherited by all ports and links.
But physical location also applies to entities that are not devices in the
routing sense:

- Furniture (desks, chairs, shelves, racks) — see §2.11
- Rooms and spaces
- Cable runs
- Wall-mounted items that are not electronic (whiteboards, acoustic panels)
- People (inferred zone, not tracked position)

These non-device entities exist in the physical model but MUST NOT appear in
the routing graph. They participate in the 3D scene and in zone/location
inference, but the router MUST NOT build pipelines through them.

### Port

A port is a typed endpoint on a device. Ports MUST be directional — they are
either sources (produce data) or sinks (consume data). A port describes what it
can accept or produce.

Every port MUST have an `id`, `device_id`, `direction`, `media_type`,
`capabilities`, and `current_state`.

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

#### PortState

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
The routing graph MUST include all of them — the router SHOULD choose which
level to connect at based on what is available and what the pipeline requires.

### Link

A link is a connection between a source port and a sink port. Links carry data
and MUST have measurable properties.

Every link MUST have an `id`, `source`, `sink`, `transport`, `direction`, and
`state`.

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

#### LinkState

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

Link status values MUST be one of the following:

- `active` — data is flowing through this link right now
- `warm` — link is initialised and ready (process running, connection established,
  negotiation complete) but no data is flowing. Activation from warm MUST be
  near-zero.
- `standby` — link exists in the graph but is not initialised. Activation
  requires startup (process launch, negotiation, handshake).
- `failed` — link was active or warm but has broken
- `unknown` — state cannot be determined

#### BandwidthSpec

```yaml
BandwidthSpec:
  capacity_bps: uint64          # maximum theoretical bandwidth
  available_bps: uint64         # currently available (capacity minus other users)
  used_bps: uint64              # currently consumed by this link's data
  quality: InfoQuality          # see §5
```

A BandwidthSpec MUST include `capacity_bps`, `available_bps`, `used_bps`, and
`quality`.

#### LatencySpec

```yaml
LatencySpec:
  min_ms: float                 # best case
  typical_ms: float             # median / p50
  max_ms: float                 # worst case / p99
  quality: InfoQuality
```

A LatencySpec MUST include `min_ms`, `typical_ms`, `max_ms`, and `quality`.

#### JitterSpec

```yaml
JitterSpec:
  mean_ms: float
  p95_ms: float
  p99_ms: float
  quality: InfoQuality
```

A JitterSpec MUST include `mean_ms`, `p95_ms`, `p99_ms`, and `quality`.

#### LossSpec

```yaml
LossSpec:
  rate: float                   # 0.0–1.0, fraction of packets/frames lost
  window_seconds: uint          # measurement window
  quality: InfoQuality
```

The `rate` field MUST be in the range 0.0 to 1.0 inclusive. A LossSpec MUST
include `rate`, `window_seconds`, and `quality`.

#### ActivationTimeSpec

See §2.6 for full discussion.

```yaml
ActivationTimeSpec:
  cold_to_warm_ms: float        # time to initialise (process start, negotiation, HDCP, etc.)
  warm_to_active_ms: float      # time to start data flow once initialised
  active_to_warm_ms: float      # time to stop data but keep ready
  warm_to_standby_ms: float     # time to tear down
  quality: InfoQuality
```

An ActivationTimeSpec MUST include all four timing fields and `quality`.

#### Link categories

Links MAY be:

- **Physical**: HDMI cable, USB cable, audio cable
- **Logical**: Network path (UDP between two hosts), WireGuard tunnel, Connect relay
- **Virtual**: Software pipe, PipeWire link, loopback
- **Exotic**: HDMI output to HDMI capture (loopback transport), USB gadget composite

A link between two ports on the **same device** is an internal link. Internal
links model the signal path through compound devices (e.g., HDMI input to USB
output inside a capture card). Their properties SHOULD be `spec` or `reported`
quality.

### Pipeline

A pipeline is an ordered chain of links assembled by the router to carry a
signal from a source to a destination, satisfying a declared intent.

Every pipeline MUST have an `id`, `intent`, `source`, `destination`, `hops`,
`aggregate`, `state`, and `warmth_policy`.

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

#### PipelineHop

Each hop MUST reference the link traversed, and MUST declare the input and
output formats. If the formats differ, the hop MUST reference the converter
used.

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

#### PipelineMetrics

PipelineMetrics are computed, not stored — they MUST be derived from hop
properties.

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
MUST compute pipelines from the graph, the available links, and the declared
intent. Pipelines MUST be re-evaluated when the graph changes (device
added/removed, link metrics change, intent changes).

**Multi-stream**: A single user action (e.g., switching to a scenario) MAY
require multiple pipelines — one for video, one for audio, one for HID. These
are independent pipelines with independent intents. They MAY share some links
(same network path) but MUST be negotiated separately.

**Fan-out**: A single source port MAY feed multiple pipelines simultaneously.
Each pipeline has its own intent and MAY negotiate a different format. The
source MUST produce once; conversion happens per-pipeline where needed. Fan-out
is how preview thumbnails, recording, and broadcast coexist with the primary
user session.

#### WarmthPolicy

To avoid activation time on scenario switches, pipelines MAY be kept **warm** —
initialised and ready but not actively flowing data. This trades resources
(running processes, open connections, memory) for faster switching.

```yaml
WarmthPolicy:
  keep_warm: bool               # should this pipeline be kept warm when not active?
  warm_priority: uint           # if resources are limited, which warm pipelines to keep
  max_warm_duration_s: uint?    # auto-cool after this long (null = indefinite)
  warm_cost: WarmCost           # resource cost of keeping this pipeline warm
```

A WarmthPolicy MUST include `keep_warm` and `warm_priority`. The
`max_warm_duration_s` field is OPTIONAL; if omitted, the pipeline MAY remain
warm indefinitely. The `warm_cost` field SHOULD be provided so the router can
make informed decisions about which pipelines to keep warm when resources are
limited.

**WarmCost** (informational — helps the router decide what to keep warm):

```yaml
WarmCost:
  cpu_percent: float?           # estimated CPU usage while warm (idle process)
  memory_mb: float?             # estimated memory usage while warm
  bandwidth_bps: uint64?        # any keepalive traffic
  gpu_slots: uint?              # hardware codec sessions held open
  description: string?          # human-readable cost summary
```

All fields in WarmCost are OPTIONAL. Implementations SHOULD provide as many
fields as can be reasonably estimated.
