# Routing Protocol Specification

**Version**: 0.1 (unstable)
**Status**: Draft — defines the model from which all other protocol work derives

---

## 1. Overview

Ozma is a software-defined KVMA router. The routing protocol defines how
signals — video, audio, peripherals, data, power, and control — are modelled,
discovered, negotiated, and delivered across an arbitrarily complex graph of
devices, transports, and conversions.

The core premise: every path through the system — from a keyboard through a
USB cable, across a network, into a USB gadget on a target machine; from a
wall outlet through a UPS, PDU, and PSU to a CPU core; from a microphone
through a mix bus, insert chain, and network transport to a remote speaker —
is a graph of typed ports connected by links. Each link has measurable
properties (bandwidth, latency, jitter, loss, voltage, current). Each port
has discoverable capabilities (supported formats, maximum throughput, power
budget). The router's job is to assemble the best pipeline through this
graph to satisfy a declared **intent**.

### What the graph model covers

The specification models the complete physical and logical infrastructure of
any computing environment — from a single PC on a desk to a multi-hall
datacentre:

**Signal routing**: Video (HDMI/DP/NDI/HLS/WebRTC), audio (PCM/VBAN/Opus/
AES67 with pro audio mixing, spatial speaker arrangements, room correction),
HID (keyboard/mouse/gamepad), RGB lighting, screen content, control surface
I/O — all as pipelines assembled from graph primitives with format negotiation,
bandwidth calculation, and latency budgeting.

**Device topology**: Every device — from an SFP+ transceiver to a
motherboard to a Thunderbolt dock to an AV receiver to a 24-bay SAS JBOD —
is a compound device with ports, internal links, and discoverable
capabilities. USB controller mapping, PCIe lane sharing, chipset uplink
bottlenecks, GPU display engine head/PLL constraints, DIMM channel
interleaving, and SAS expander zoning are all expressed in the graph.

**Power**: Every voltage rail traced from utility feed through UPS, PDU,
PSU, and regulator to every consumer. Per-port USB power budgets. PoE
allocation. RGB LED current calculation. Barrel jack specifications with
polarity and actual-vs-labelled voltage. Modular PSU cable pinout safety.
Battery state for every device that has one.

**Physical environment**: Sites, buildings, rooms (including datacentre
halls with hot/cold aisle containment and A/B power feeds), rack rows,
racks (including the LackRack), rack units with occupancy, patch panels
with per-port structured cabling documentation, furniture with relative
positioning, spatial zones for routing decisions.

**Monitoring**: Every property in the graph — resource utilisation, link
health, power state, thermal readings, firmware versions — is observable
in real time, recorded in a persistent journal, trended for predictive
alerting, and exportable to external systems. The routing graph is a
monitoring platform by construction: the data needed for routing decisions
is exactly the data a monitoring platform needs.

**Asset management**: Every device has hardware identity (serial numbers,
MAC addresses, UUIDs), firmware tracking (including BIOS/AGESA/microcode
version history with known issues), and a complete lifecycle journal.
Compatibility checking validates physical fitment, electrical compatibility,
bandwidth constraints, power adequacy, pinout safety, and thermal clearance
across any combination of components.

**Audio production**: Mix buses, monitor controller (source selection,
speaker switching, dim, mono, talkback), insert chains with processor
ordering, cue sends, per-app audio separation, metering (peak/LUFS/VU),
gain staging, dither, spatial audio with speaker arrangement and listener
position, room acoustics model, Dante-equivalent clock sync on commodity
Ethernet (PTP with hardware timestamps), and full PipeWire integration.

**Control**: Every device has a control path — how commands reach it,
through what intermediary, with what dependency chain. DDC/CI via display
cable, CEC via HDMI, serial via USB adapter on a specific node, IP via
vendor API, IR via blaster — all with reachability tracking and fallback
paths.

### Scale independence

The same primitives model:

| Scale | Example |
|-------|---------|
| Single device | One PC — USB topology, audio routing, storage health, thermal monitoring |
| Desk | PC + monitors + speakers + peripherals + desk lamp — spatial audio, RGB, power budget |
| Room | Multiple desks + rack + AV receiver + cameras — KVM switching, network, AV routing |
| Building | Multiple rooms + structured cabling + WiFi + IoT — fleet management, VLAN, cooling |
| Campus | Multiple buildings + inter-building fiber + WAN — federation, remote access, Connect relay |
| Datacentre | Halls + rack rows + A/B power + CRAC cooling — full infrastructure graph |

No model changes between scales. A `Device` is a `Device` whether it's a
USB keyboard or a 48-port spine switch. A `Link` is a `Link` whether it's
a 10cm USB cable or a 40km singlemode fiber run. A `VoltageRail` is a
`VoltageRail` whether it's a 5V USB port or a 480V 3-phase utility feed.
The graph grows; the primitives don't change.

### Plugin extensibility

Everything that is not a core graph primitive is a plugin — transports,
device discovery, codecs, converters, switch controllers. Third-party
plugins register new transport types, device classes, and capabilities at
runtime. The plugin interface is Python and stable — plugins written today
will continue to work when the controller core is rewritten in Rust (via
PyO3 embedded interpreter). Built-in transports and community-contributed
LoRa/Zigbee/Dante plugins use the same interface.

### This specification defines:

1. **Graph primitives** — Device, Port, Link, Pipeline, with hardware identity, physical location, power profile, and control path on every entity
2. **Intents** — what the user wants to achieve (gaming, creative, desktop, fidelity audio, broadcast), with constraints, preferences, degradation policies, and automatic binding to observed conditions
3. **Format system** — video, audio (with channel mapping), HID, screen, RGB, control surface, and data formats with three-phase negotiation (enumerate → restrict → fixate)
4. **Information quality** — seven trust levels (user → measured → inferred → reported → commanded → spec → assumed) with provenance tracking and temporal decay
5. **Route calculation** — cost model with device pressure, spatial zones, and activation time; constraint satisfaction; remediation with safety levels; intent bindings
6. **Plugin contracts** — registration, lifecycle, language stability guarantee; interfaces for transports, devices, codecs, converters, switches
7. **Clock model** — PTP with hardware timestamps (Dante-equivalent on commodity Ethernet), configurable jitter buffers, drift compensation, sample-accurate sync
8. **Topology discovery** — five-layer discovery (hardware → OS interface → network → measurement → enrichment), opaque device handling, topology calibration probes
9. **Device versioning** — Ozma software + third-party firmware (LVFS/fwupd), BIOS/AGESA/microcode tracking with known issue database, fleet update orchestration
10. **Power model** — voltage rails with measured/inferred current, USB PD negotiation state, PoE, per-function power cost, RGB current calculation, power distribution (PDU/UPS/strips with per-outlet metering and switching)
11. **Physical environment** — sites, spaces (offices through datacentre halls), spatial zones, furniture (desks through 42U racks), relative positioning, hot/cold aisle containment, A/B power feeds
12. **Control path** — how commands reach devices through intermediaries, dependency chains, reachability tracking, fallback paths, cloud control risk surfacing
13. **Audio routing** — mix buses, monitor controller, insert chains, cue sends, metering (LUFS/peak/VU), gain staging, dither, spatial audio with speaker arrangement and room acoustics, active redundancy, per-app audio separation, platform virtual audio devices
14. **Thermal and power management** — fan curves (zone-aware, cause-aware, noise-aware), power profiles (CPU/GPU/platform with intent-driven switching), thermal-aware routing (encode placement, storage path selection, noise-sensitive fan capping)
15. **Physical device database** — universal open catalog: motherboards with physical port mapping and chipset topology, CPUs with iGPU/NPU, GPUs with display engine constraints and codec sessions, RAM with XMP/channel topology, storage with SAS/NVMe/HBA/enclosure/backplane, monitors with compound device model (KVM/hub/speakers/DDC-CI), AV receivers, pro audio interfaces, SFP transceivers with EEPROM/DOM/firmware/recode, cables/adapters/risers with bandwidth constraints, power connectors with pinout/polarity/actual voltage, racks with accessories and patch panels — all community-contributed via Connect
16. **Node definition** — complete specification of Ozma nodes: platform, target binding, USB gadget composition, GPIO/I2C/SPI peripheral buses, services, network/mesh identity, lifecycle
17. **Observability** — real-time metrics, historical journal, trend analysis, Sankey flow diagrams, asset inventory with serial numbers and lifecycle tracking, fleet firmware state, compatibility checking with build validation

### Decentralised decision-making

The specification defines a global model but does not require global
knowledge for every decision. Routing decisions are made at the level that
has sufficient context:

**A node** knows its own USB topology, its connected devices, its thermal
state, its power budget, and its link to the controller. It can make local
decisions: which USB port to recommend for a capture card, when to ramp
its fans, whether its power budget can sustain another device. It doesn't
need to know about the datacentre's CRAC units or the chipset topology of
a server three racks away.

**A controller** knows its local mesh — the nodes it manages, the links
between them, the scenarios configured, the audio routing. It makes
pipeline decisions for its scope: which node gets HID, which codec to use,
when to failover. It doesn't need to know the internal topology of a
remote controller's mesh.

**Connect** sees across meshes — device populations, firmware distribution,
provenance chains, fleet health trends. It makes platform-level decisions:
which firmware updates to distribute, which device database entries are
popular, aggregate failure patterns.

**The graph is composable, not monolithic.** Each participant holds the
subgraph relevant to its decisions. Subgraphs overlap at boundaries
(a controller knows its nodes; a node knows its devices; the device
database provides specs for both). More data enables better decisions —
a controller with chipset topology data can identify DMI bottlenecks that
a node without that data can't — but the system functions at every level
of knowledge. A node with no device database match still routes traffic
using `assumed` quality properties. A controller with no chipset data
still switches scenarios. The model degrades gracefully as information
decreases.

This is the same principle as `InfoQuality` (§5): the system makes the
best decision possible with the data available, tracks the confidence of
that decision, and improves as better data arrives. A routing decision
based on `assumed` properties works. A routing decision based on `measured`
properties works better. Neither requires the other to exist.

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

**HardwareIdentity** — every discoverable unique identifier for this device:

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
  # Devices may have multiple identifiers from different sources.
  # The `id` field on Device is Ozma's stable identifier (survives
  # reconnection). HardwareIdentity carries the raw hardware serials
  # for asset tracking, warranty lookup, and deduplication.
  #
  # Discovery priority: USB serial > SMBIOS serial > MAC > PCI ID > user-assigned.
  # If a device has no discoverable serial, the user can assign one manually
  # (e.g., reading the serial sticker on the back of a monitor).
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
| `ups` | UPS — battery backup, power conditioning, load monitoring |
| `pdu` | Power Distribution Unit — rack or desk, metered/switched/basic |
| `power_strip` | Power strip, surge protector, extension cord |
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

  # --- Power input connector (the physical power port) ---
  power_input: PowerConnectorSpec?      # what connector and what it actually needs
  power_inputs: PowerConnectorSpec[]?   # if multiple power inputs (redundant PSU, AC+USB)

  # For devices that deliver power:
  output_rails: OutputRail[]?

  # For batteries:
  battery_spec: BatterySpec?

  # For LED devices:
  led_power: LedPowerSpec?

PowerConnectorSpec:
  connector: string             # physical connector type (see table below)
  label_voltage_v: float?       # what's printed on the device label
  actual_voltage_v: float?      # what the device actually needs (may differ from label!)
  actual_voltage_range: VoltageRange?  # full acceptable range
  current_ma: float?            # required/rated current
  power_w: float?               # wattage rating
  polarity: string?             # "center_positive", "center_negative", "ac" (for barrel jacks)
  barrel: BarrelJackSpec?       # barrel jack dimensions (if barrel_dc connector)
  iec: IecSpec?                 # IEC connector details (if IEC)
  country_plug: string?         # wall plug standard if hardwired ("nema_5_15", "type_g", etc.)
  included_adapter: PowerAdapterSpec?  # PSU/adapter that ships with the device
  compatible_adapters: string[]?  # device database IDs of known compatible adapters
  incompatible_adapters: string[]?  # adapters known to NOT work (wrong voltage, wrong polarity)
  notes: string?                # free-text notes ("label says 9V but ships with 12V adapter")
  source: string?               # where this info came from ("label", "measured", "manual",
                                # "community_verified")

BarrelJackSpec:
  outer_diameter_mm: float      # outer barrel diameter
  inner_diameter_mm: float      # inner pin diameter
  barrel_length_mm: float?      # barrel depth
  # Common sizes:
  # 5.5×2.1mm — most common (Arduino, LED strips, many devices)
  # 5.5×2.5mm — many laptops, some pro audio
  # 4.0×1.7mm — some small devices
  # 3.5×1.35mm — smaller devices
  # 6.3×3.0mm — some older equipment
  # 2.1 and 2.5mm inner pins physically fit the same 5.5mm barrel —
  # a 2.5mm plug in a 2.1mm jack makes intermittent contact.
  # A 2.1mm plug in a 2.5mm jack wobbles and may not connect.
  # The database captures exact dimensions to warn about this.

IecSpec:
  type: string                  # IEC 60320 type
  temperature_rating_c: uint?   # max temperature (C15/C17 = 120°C, C13 = 70°C)
  current_rating_a: float       # rated current
  voltage_rating_v: uint        # rated voltage (typically 250V AC)
  fused: bool?                  # fused connector (UK C13 with built-in fuse)
  locking: bool?                # locking variant (IEC 60320-1 C13L/C19L)
  # IEC types that look similar but are NOT interchangeable:
  # C13 (70°C, 10A) vs C15 (120°C, 10A) — C15 has a notch, for hot devices
  # C19 (70°C, 16A) vs C21 (120°C, 16A) — C21 has a notch
  # Using a C13 cable on a device that needs C15 (e.g., kettle, high-temp
  # equipment) is a fire hazard — the cable isn't rated for the temperature.

PowerAdapterSpec:
  type: string                  # "ac_adapter" (wall wart), "inline_psu", "usb_charger",
                                # "internal_psu", "open_frame_psu"
  input_voltage: string?        # "100-240V AC" (universal), "120V AC", "230V AC"
  input_frequency: string?      # "50/60Hz", "50Hz", "60Hz"
  output_voltage_v: float       # DC output voltage
  output_current_ma: float      # DC output current
  output_power_w: float?        # output power
  output_connector: string      # connector type on the output
  output_barrel: BarrelJackSpec?  # barrel dimensions if barrel_dc
  output_polarity: string?      # "center_positive", "center_negative"
  regulation: string?           # "regulated", "unregulated"
  # Unregulated adapters output higher voltage at low load (a "12V"
  # unregulated adapter may output 15V with no load, 12V at rated current,
  # and 10V when overloaded). Regulated adapters maintain constant voltage.
  efficiency: string?           # "level_vi" (modern efficient), "level_v", "linear"
  standby_power_w: float?       # power consumed when device is off/standby
  replacement_available: bool?  # can you still buy this adapter?
  generic_compatible: bool?     # can a generic adapter with matching specs work?

# Example — your FireWire mixer that says 9V but needs 12V:
#
# power_input:
#   connector: "barrel_dc"
#   label_voltage_v: 9          # WRONG — label is incorrect
#   actual_voltage_v: 12        # what it actually needs
#   actual_voltage_range: { nominal: 12, min: 11, max: 13 }
#   current_ma: 1500
#   polarity: "center_positive"
#   barrel: { outer_diameter_mm: 5.5, inner_diameter_mm: 2.1 }
#   included_adapter:
#     output_voltage_v: 12      # ships with 12V despite 9V label
#     output_current_ma: 2000
#     output_connector: "barrel_dc"
#     output_polarity: "center_positive"
#   notes: "Device label reads 9V DC but ships with 12V adapter and
#           requires 12V to operate. 9V supply causes erratic behaviour.
#           Community verified."
#   source: "community_verified"
```

**Why this matters**:

1. **Second-hand equipment**: When you buy a mixer/interface/device without
   its original adapter, you need to know the actual voltage, current,
   polarity, and barrel size. The label might be wrong. The device database
   is the source of truth — community-verified, not manufacturer labels.

2. **Cable/adapter shopping**: "I need a replacement power cable for my
   monitor" — the database tells you it's IEC C14 (not C8, not C6),
   10A rated. "I need a power supply for this LED strip" — 5V, 10A,
   5.5×2.1mm center-positive barrel.

3. **Polarity warnings**: Center-negative barrel jacks exist (some older
   effects pedals, some Yamaha keyboards). Plugging in a center-positive
   adapter can damage the device. The database captures polarity per device
   and warns on mismatch.

4. **IEC temperature ratings**: A C13 cable on a device that generates heat
   (laboratory equipment, some industrial gear) is a fire hazard if the
   device needs C15 (120°C rated). They look almost identical — C15 has
   a small notch. The database knows which type is required.

5. **Universal adapter compatibility**: The database tracks which devices
   can use generic adapters (voltage + current + polarity + barrel match)
   vs which need specific vendor adapters (proprietary connectors, specific
   regulation requirements, communication pins).

6. **Complete cable inventory**: With power connectors modelled alongside
   data connectors, the system can generate a complete cable shopping list
   for any setup: "Your rack needs: 3× IEC C13-C14 cables (1.8m), 2×
   IEC C19-C20 cables (1.0m), 1× 12V/3A 5.5×2.1mm center-positive
   barrel adapter, 4× Cat6a patch cables (0.5m), 2× SFF-8643 SAS cables
   (1.0m)."

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

**PowerDistributionSpec** (PDUs, UPS, power strips, surge protectors,
extension cords, splitters — anything between the wall and the equipment):

These are power routing devices — they sit in the power graph and distribute
mains or DC power to downstream devices. They have input capacity, output
outlets with individual ratings, and potentially monitoring, switching, and
battery backup. They are the power equivalent of a network switch.

```yaml
PowerDistributionSpec:
  device_type: string           # "ups", "pdu_basic", "pdu_metered", "pdu_switched",
                                # "pdu_managed", "power_strip", "surge_protector",
                                # "extension_cord", "dc_splitter", "dc_distribution"
  input: PowerDistInput         # power input (what feeds this device)
  outputs: PowerDistOutput[]    # power outputs (what this device feeds)
  total_capacity: PowerCapacity # total rated capacity
  monitoring: PowerMonitoring?  # metering/monitoring capabilities
  switching: PowerSwitching?    # per-outlet switching (if supported)
  surge: SurgeSpec?             # surge protection (if any)
  ups: UpsSpec?                 # battery backup (if UPS)
  cascading: CascadingSpec?     # daisy-chain / cascading rules
  cord_length_m: float?         # for extension cords and strips
  mounting: string?             # "rack_horizontal", "rack_vertical", "rack_0u",
                                # "wall", "desk", "floor", "none"
  rack_units: uint?             # for rack-mount PDUs (0U = vertical mount)

PowerDistInput:
  connector: string             # "iec_c14", "iec_c20", "nema_5_15", "nema_l6_30",
                                # "type_g", "type_f_schuko", "hardwired",
                                # "barrel_dc", "terminal_block"
  voltage: string               # "120v_ac", "230v_ac", "100-240v_ac", "208v_ac",
                                # "12v_dc", "24v_dc", "48v_dc"
  phase: string?                # "single", "three_phase", "split_phase"
  max_current_a: float          # input breaker/fuse rating
  max_power_w: float?           # total input power rating
  frequency_hz: string?         # "50", "60", "50/60"
  plug: PowerConnectorSpec?     # the physical plug (for cable shopping)
  cord_length_m: float?         # input cord length

PowerDistOutput:
  id: string                    # "outlet_1", "outlet_a1", "bank_a_1"
  connector: string             # output receptacle type
  bank: string?                 # which bank/group ("A", "B", "all")
  max_current_a: float?         # per-outlet rating (may be less than total)
  switched: bool?               # can this outlet be switched on/off?
  metered: bool?                # is power metered on this outlet?
  always_on: bool?              # designated always-on (UPS: on battery too)
  battery_backed: bool?         # UPS: on battery during outage?
  physical: PhysicalPortInfo?   # position on the device
  connected_device: string?     # device database ID of what's plugged in (user-configured)
  # connected_device enables the power graph: wall → PDU outlet 3 → server PSU.
  # The router traces power from wall to every device.

PowerCapacity:
  max_current_a: float          # total rated current
  max_power_va: float?          # total rated VA (apparent power)
  max_power_w: float?           # total rated watts (real power)
  outlets_total: uint           # total outlet count
  outlet_types: OutletCount[]?  # breakdown by type

OutletCount:
  connector: string             # "iec_c13", "iec_c19", "nema_5_15", "nema_5_20",
                                # "type_g", "type_f_schuko", "usb_a", "usb_c"
  count: uint
  max_current_a: float?         # per-outlet of this type

PowerMonitoring:
  type: string                  # "none", "total_input", "per_bank", "per_outlet"
  metrics: string[]?            # ["voltage", "current", "power_w", "power_va",
                                # "power_factor", "energy_kwh", "frequency",
                                # "temperature", "humidity"]
  protocol: string?             # "snmp", "http", "serial", "modbus", "nut",
                                # "apc_ap9630", "raritan_px", "servertech_sentry",
                                # "cyberpower_cloud", "eaton_ipp"
  # Per-outlet metered PDUs provide measured power per device — this feeds
  # directly into the power model (§2.10) as `measured` quality data.
  # A metered PDU is a fleet of INA219 sensors for AC power.

PowerSwitching:
  type: string                  # "none", "per_outlet", "per_bank", "master"
  remote: bool?                 # switchable via network/serial (not just physical button)
  scheduled: bool?              # supports scheduled on/off
  delay_sequencing: bool?       # staggered power-on to avoid inrush current spike
  default_state: string?        # "last_state", "always_on", "always_off"
  # Remote-switched outlets enable: graceful shutdown sequencing (storage
  # before compute), power cycling as remediation (§8.6), and scheduled
  # power for non-essential equipment.

SurgeSpec:
  joules: uint?                 # surge energy absorption rating
  clamping_voltage_v: float?    # voltage at which protection activates
  response_time_ns: float?      # surge response time
  protection_indicators: bool?  # LED showing protection status
  protection_modes: string[]?   # ["line_to_neutral", "line_to_ground", "neutral_to_ground"]
  emi_rfi_filter: bool?         # EMI/RFI noise filtering
  coax_protection: bool?        # coax/antenna surge protection
  ethernet_protection: bool?    # Ethernet surge protection
  phone_protection: bool?       # phone/DSL line protection
  connected_equipment_warranty: float?  # vendor warranty on connected equipment ($)
  # Surge ratings degrade over time as the MOV absorbs surges.
  # The trend analysis (§11.7) can track if a smart surge protector
  # reports declining protection capacity.

UpsSpec:
  topology: string              # "standby" (offline), "line_interactive", "online_double_conversion"
  capacity_va: uint             # rated VA capacity
  capacity_w: uint?             # rated watt capacity (typically 60% of VA)
  battery: BatterySpec          # battery type, capacity, chemistry
  runtime_minutes: RuntimeEstimate[]?  # estimated runtime at various load levels
  transfer_time_ms: float?      # time to switch to battery (standby: 5-12ms,
                                # line-interactive: 2-4ms, online: 0ms)
  input_voltage_range: VoltageRange?  # input voltage tolerance before switching to battery
  avr: bool?                    # automatic voltage regulation (boosts/bucks without battery)
  pure_sine_wave: bool?         # pure sine output (vs simulated/stepped sine)
  # Simulated sine can cause problems with active PFC PSUs (most modern PSUs).
  # Pure sine is required for reliable operation of computer equipment.
  outlets_battery: uint?        # outlets on battery backup
  outlets_surge_only: uint?     # outlets with surge only (no battery)
  usb_port: bool?               # USB for NUT/apcupsd communication
  serial_port: bool?            # serial for NUT/apcupsd
  network_card: bool?           # SNMP network management card (slot or built-in)
  network_protocol: string?     # "snmp", "apc_smartconnect", "eaton_ipp", "cyberpower_cloud"
  display: bool?                # LCD/LED status display
  audible_alarm: bool?          # beep on battery/overload
  self_test: bool?              # automatic self-test capability
  firmware: FirmwareInfo?       # UPS firmware (updateable on some models)

  # Generator compatibility
  generator_compatible: bool?   # works with generator power (frequency tolerance)
  frequency_tolerance_hz: float?  # input frequency range (generators may drift)

RuntimeEstimate:
  load_percent: uint            # load as percentage of capacity
  load_w: uint?                 # load in watts
  runtime_min: float            # estimated minutes of battery runtime

CascadingSpec:
  max_daisy_chain: uint?        # max units chained (some power strips have pass-through outlets)
  passthrough_outlet: bool?     # has an outlet that passes through to chain
  # Daisy-chaining power strips is generally unsafe and violates electrical
  # codes in most jurisdictions. The compatibility engine (§15.13) should
  # warn if the user models a chain: "Power strip connected to another
  # power strip. This is unsafe and may violate local electrical codes."
  # PDUs are different — rack PDUs are designed for high-density deployment
  # and don't have the same daisy-chain issues.
```

**Power distribution in the routing graph**:

Power distribution devices sit in the power graph between the wall outlet
and the equipment. The graph traces power from source to every consumer:

```
Wall outlet (NEMA 5-15, 120V/15A = 1800W max on this circuit)
└── UPS: APC Smart-UPS 1500 (line-interactive, 1000W)
    ├── Battery outlets:
    │   ├── Outlet 1 → Server PSU (measured: 280W) [via IEC C13 cable]
    │   ├── Outlet 2 → Network switch (measured: 25W) [via IEC C13 cable]
    │   └── Outlet 3 → NAS (measured: 65W) [via IEC C13 cable]
    │   Total battery-backed: 370W of 1000W capacity (37% load)
    │   Estimated runtime on battery: 22 minutes
    │
    └── Surge-only outlets:
        ├── Outlet 4 → Monitor (measured: 45W)
        ├── Outlet 5 → Desk lamp (15W)
        └── Outlet 6 → Phone charger (20W)

Wall outlet 2 (same circuit — shares 1800W with outlet 1!)
└── Power strip: Belkin 12-outlet surge protector (1875W, 4320J)
    ├── Outlet 1 → Audio interface PSU (30W)
    ├── Outlet 2 → Monitor 2 (45W)
    ├── Outlet 3 → Powered speakers (2× 50W = 100W)
    ├── Outlet 4 → LED strip PSU (60W)
    └── Outlet 5–12: empty

Total circuit load: 370W + 80W + 235W = 685W of 1800W (38%)
```

The router surfaces this as a power Sankey diagram (§11.2). It knows:
- Total circuit capacity and current load
- UPS battery runtime at current load
- Which devices lose power on outage (surge-only outlets)
- Which devices stay up (battery-backed outlets)
- Whether adding a new device would overload the UPS or the circuit
- Per-outlet power via metered PDU (if available) or estimated from
  device database power specs

**UPS integration with Ozma**:

The existing NUT integration (`controller/ups_monitor.py`) feeds UPS state
into the graph. With the full UPS model:

1. **Runtime estimation**: "At current load (370W), battery runtime is
   22 minutes. If the server starts a heavy encode (450W total), runtime
   drops to 14 minutes."

2. **Graceful shutdown sequencing**: On battery, after X minutes: shut down
   non-essential devices first (switched PDU outlets), then gracefully
   shut down the server, then the NAS. The switched PDU and the UPS
   coordinate via the power graph.

3. **Overload prevention**: "Adding this GPU server (600W) to the UPS would
   exceed its 1000W capacity. Move it to a dedicated circuit or upgrade
   the UPS."

4. **Circuit-level awareness**: Two UPS units on the same wall circuit share
   the circuit's amperage limit. The graph models this — both UPS inputs
   trace to the same circuit.

5. **Battery health trending**: UPS battery capacity degrades over time.
   NUT reports remaining capacity. Trend analysis (§11.7): "UPS battery
   capacity has dropped 15% in the last year. Runtime at full load is now
   14 minutes vs 22 minutes when new. Consider battery replacement."

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
  floor_plan: FloorPlan?         # physical layout with walls, doors, materials (for RF modelling)
  datacentre: DatacentreSpaceSpec?  # datacentre-specific properties (if applicable)

DatacentreSpaceSpec:
  # --- Rack layout ---
  rows: RackRow[]?              # rack rows in this space
  hot_cold_aisle: bool?         # hot/cold aisle arrangement
  containment: string?          # "hot_aisle_containment", "cold_aisle_containment",
                                # "chimney", "none"
  raised_floor: bool?           # raised floor (cable routing, cold air plenum)
  raised_floor_height_mm: uint? # clearance under raised floor
  overhead_cable_tray: bool?    # overhead cable trays / ladder rack

  # --- Power infrastructure ---
  power_feeds: PowerFeed[]?     # utility/generator feeds to this space
  power_redundancy: string?     # "single_feed", "a_b_redundant", "2n",
                                # "2n_plus_1", "concurrent_maintainable"
  total_power_kw: float?        # total available power for this space
  pue: float?                   # Power Usage Effectiveness (total facility / IT load)
                                # PUE 1.0 = perfect. Typical DC: 1.3–1.6. Good: 1.1–1.2.

  # --- Cooling ---
  cooling_type: string?         # "crac" (computer room AC), "crah" (air handler),
                                # "in_row", "rear_door_heat_exchanger", "liquid",
                                # "free_air", "evaporative"
  cooling_capacity_kw: float?   # total cooling capacity
  target_temp_c: float?         # cold aisle target temperature
  humidity_range: { min: float, max: float }?  # relative humidity range (%)

  # --- Physical security ---
  access_control: string?       # "badge", "biometric", "mantrap", "none"
  cameras: bool?                # CCTV monitoring
  fire_suppression: string?     # "sprinkler", "fm200", "novec_1230", "inergen", "none"

RackRow:
  id: string                    # "row_a", "row_1"
  name: string?
  orientation: string?          # "north_south", "east_west"
  racks: string[]               # rack device IDs in this row, in order
  aisle: string?                # which aisle this row faces ("hot", "cold")
  power_feed: string?           # which power feed serves this row ("a", "b")

PowerFeed:
  id: string                    # "feed_a", "feed_b", "generator"
  source: string                # "utility", "generator", "solar", "ups_output"
  voltage: string               # "208v_3phase", "480v_3phase", "240v_single"
  capacity_a: float             # amperage capacity
  capacity_kw: float?           # kilowatt capacity
  redundant_with: string?       # which other feed provides redundancy
  transfer_switch: string?      # "ats" (automatic transfer switch), "sts" (static), "manual"
  transfer_time_ms: float?      # switchover time (ATS: 10-20ms, STS: 4-8ms, manual: minutes)
  # A+B redundancy means every rack has two power feeds from different
  # UPS systems, and every server has dual PSUs — each on a different feed.
  # If feed A fails, feed B carries 100% of the load.

SpaceType: enum
  office                        # private office
  open_plan                     # open plan workspace
  meeting_room                  # conference / meeting room
  lab                           # computer lab, workshop
  studio                        # recording / production studio
  living_room                   # residential living area
  bedroom                       # residential bedroom
  server_room                   # dedicated server/network room
  data_hall                     # datacentre compute hall (rows of racks)
  network_room                  # MDF/IDF, meet-me room, cross-connects
  power_room                    # UPS room, PDU switchboards, generator, battery
  cooling_plant                 # CRAC/CRAH units, chiller plant
  loading_dock                  # receiving, staging
  utility_room                  # storage, mechanical
  outdoor                       # patio, garden, yard
  classroom                     # teaching space
  common_area                   # kitchen, break room, hallway
  custom                        # anything else
```

**FloorPlan** — physical layout of a space with walls, doors, windows, and
construction materials. Enables WiFi coverage prediction, LoRa/BLE range
estimation, audio acoustics modelling, and cable run planning:

```yaml
FloorPlan:
  source: string?               # "manual", "imported_dxf", "imported_image",
                                # "imported_pdf", "3d_scan"
  scale: float?                 # if traced from image: mm per pixel
  origin: Position2d?           # floor plan coordinate origin
  walls: Wall[]
  doors: Door[]?
  windows: Window[]?
  floors: FloorSurface[]?       # floor material (affects acoustics + cable routing)
  ceilings: CeilingSurface[]?   # ceiling material and height
  columns: Column[]?            # structural columns
  cable_paths: CablePath[]?     # known cable routes (conduit, trunking, floor void, overhead)

Wall:
  id: string
  start: Position2d             # start point (mm)
  end: Position2d               # end point (mm)
  height_mm: float?             # wall height (default: room height)
  thickness_mm: float?          # wall thickness
  material: WallMaterial        # construction material (determines RF attenuation)
  exterior: bool?               # is this an external wall?
  load_bearing: bool?           # structural (can't be modified)

WallMaterial:
  type: string                  # material type (see RF attenuation table below)
  rf_attenuation_2_4ghz_db: float?  # signal loss at 2.4 GHz
  rf_attenuation_5ghz_db: float?    # signal loss at 5 GHz
  rf_attenuation_6ghz_db: float?    # signal loss at 6 GHz (WiFi 6E/7)
  acoustic_stc: uint?           # Sound Transmission Class (for audio leakage)
  # Custom materials can specify measured attenuation. Standard materials
  # use lookup values from the table below.

# Standard wall material RF attenuation:
#
# | Material | 2.4 GHz | 5 GHz | 6 GHz | Notes |
# |----------|---------|-------|-------|-------|
# | plasterboard (drywall) | 3 dB | 4 dB | 5 dB | Standard interior partition |
# | plasterboard_double | 5 dB | 7 dB | 9 dB | Double-layer drywall |
# | brick_single | 6 dB | 10 dB | 12 dB | Single brick (110mm) |
# | brick_double | 10 dB | 18 dB | 22 dB | Double brick cavity wall |
# | concrete_100mm | 10 dB | 15 dB | 18 dB | Poured concrete |
# | concrete_200mm | 15 dB | 23 dB | 28 dB | Thick concrete (fire wall, lift shaft) |
# | concrete_block | 8 dB | 12 dB | 15 dB | Concrete masonry unit |
# | glass_single | 2 dB | 3 dB | 4 dB | Single glazing |
# | glass_double | 4 dB | 6 dB | 8 dB | Double glazing |
# | glass_tinted | 5 dB | 8 dB | 10 dB | Tinted/coated glass (metal oxide) |
# | glass_low_e | 8 dB | 15 dB | 20 dB | Low-E glass (metallic coating — WiFi killer!) |
# | wood_door | 3 dB | 4 dB | 5 dB | Solid wood door |
# | hollow_door | 2 dB | 3 dB | 3 dB | Hollow-core interior door |
# | metal_door | 10 dB | 15 dB | 18 dB | Steel security door |
# | fire_door | 6 dB | 10 dB | 12 dB | Fire-rated door (dense core) |
# | metal_stud | 4 dB | 6 dB | 8 dB | Metal stud partition with plasterboard |
# | curtain_wall | 6 dB | 10 dB | 15 dB | Glass curtain wall (aluminium frame) |
# | elevator_shaft | 20 dB | 30 dB | 35 dB | Concrete + steel (essentially opaque) |
# | metal_clad | 15 dB | 25 dB | 30 dB | Metal-clad wall (warehouse, server room) |
# | floor_concrete | 12 dB | 18 dB | 22 dB | Concrete floor/ceiling between levels |
# | floor_wood | 5 dB | 8 dB | 10 dB | Timber floor between levels |
#
# These are typical values. Actual attenuation varies with construction
# quality, age, moisture content, and exact thickness. Community-verified
# measurements for specific buildings can override the defaults.

Door:
  id: string
  position: Position2d          # centre point on the wall
  wall_id: string               # which wall this door is in
  width_mm: float
  height_mm: float?
  material: WallMaterial        # door material (wood, metal, glass, fire-rated)
  normally_open: bool?          # is this door typically left open? (affects RF model)
  # An open door is ~0 dB attenuation. A closed hollow-core door is ~2 dB.
  # A closed fire door is ~10 dB. Whether the door is open or closed
  # significantly affects WiFi prediction. The model defaults to the
  # `normally_open` state but can be overridden by sensor data (§2.9 —
  # door contact sensor).

Window:
  id: string
  position: Position2d          # centre point on the wall
  wall_id: string               # which wall this window is in
  width_mm: float
  height_mm: float
  material: WallMaterial        # glass type (single, double, low-E, tinted)
  # Low-E glass deserves special attention — the metallic coating that
  # reflects heat also reflects RF. A building with floor-to-ceiling low-E
  # glass can have 15–20 dB attenuation at 5 GHz per window. This is why
  # modern office buildings often have terrible WiFi — the glass walls
  # that let light through block radio.

FloorSurface:
  material: string              # "concrete", "raised_floor", "hardwood", "carpet",
                                # "tile", "vinyl"
  rf_attenuation_db: float?     # between-floor attenuation (if modelling multi-storey)
  cable_accessible: bool?       # can cables be run under this floor?
  void_depth_mm: float?         # raised floor void depth (for cable routing)

CeilingSurface:
  height_mm: float              # ceiling height
  material: string              # "plasterboard", "acoustic_tile", "exposed_concrete",
                                # "suspended_grid", "exposed_structure"
  plenum: bool?                 # is the space above the ceiling a plenum?
  cable_accessible: bool?       # can cables be run above this ceiling?

Column:
  position: Position2d
  diameter_mm: float?           # for circular columns
  dimensions_mm: Dimensions?    # for rectangular columns { w, d }
  material: string              # "concrete", "steel"
  # Columns are RF obstacles — concrete columns can shadow WiFi significantly.

CablePath:
  id: string
  type: string                  # "conduit", "trunking", "floor_void", "ceiling_void",
                                # "cable_tray", "wall_chase", "external"
  waypoints: Position2d[]       # path through the building
  capacity: string?             # how many cables can this path carry
  current_usage: string?        # approximately how full
  accessible: bool              # can new cables be pulled through this path?
```

**What the floor plan enables**:

1. **WiFi coverage prediction**: Place an AP on the floor plan → the system
   calculates signal strength at every point using the wall materials and
   distances. "Your AP in the study gives -72 dBm in the bedroom (through
   2 plasterboard walls at 3+3 dB = 6 dB loss). That's marginal for 5 GHz.
   Consider a second AP or switch to 2.4 GHz for the bedroom."

2. **AP placement optimisation**: "Given these walls, the optimal placement
   for a single AP to cover all rooms is here. For two APs, place them
   here and here. Channel assignment: AP1 on channel 36, AP2 on channel
   149 to avoid co-channel interference."

3. **BLE/LoRa range estimation**: Same attenuation model, different
   frequencies. BLE beacon in the living room: "Signal reaches the garage
   through a brick wall at -85 dBm (marginal). Place a second beacon near
   the garage door."

4. **Cable run planning**: "The shortest Cat6a run from the server room
   patch panel to Office 3 is 28m via the ceiling void (accessible),
   through the corridor, down the wall chase. Cat6a supports 10GbE at
   this distance."

5. **Audio leakage**: Wall STC (Sound Transmission Class) values predict
   how much sound bleeds between rooms. "Your studio shares a plasterboard
   wall (STC 33) with the bedroom. Loud monitoring will be clearly audible
   next door. Consider adding mass-loaded vinyl for STC 45+."

6. **Security camera coverage**: Place cameras on the floor plan → calculate
   field-of-view coverage considering walls and obstructions. "This camera
   covers 80% of the hallway. The column at position (3200, 1500) creates
   a blind spot of 2.5m²."

**Floor plan import**: Users shouldn't have to draw walls from scratch.
The system should accept:
- **Image trace**: Upload a floor plan image (PDF, PNG from architect or
  real estate listing), set the scale, trace walls over the image
- **DXF/DWG import**: Import from CAD drawings (architects provide these)
- **Simple drawing**: Draw walls, doors, windows in the dashboard editor
- **3D scan import**: From LiDAR scans (iPhone Pro, Matterport) — generates
  walls and dimensions automatically
- **Template**: Common house/apartment layouts as starting points

**Accuracy**: The RF attenuation values are estimates. Real-world results
vary with construction quality, moisture, furniture, and the specific
frequency and antenna pattern. The model provides a useful prediction
(±5 dB) without requiring a professional RF survey. Community-measured
values for specific building types can improve accuracy over time.

**Physical environment is optional at every level**. A user with one desk
and two machines needs none of this — their devices just have bus-level
locations. A user who wants spatial RGB adds furniture positions. A business
with multiple rooms adds spaces and zones. An MSP managing 50 sites adds
the full hierarchy. A user who wants WiFi coverage prediction adds a floor
plan with wall materials. Each level is independently useful and none is
required.

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

### 2.14 Thermal and Power Management

The routing graph has temperature sensors, fan controls, power consumption
data, and the thermal topology linking them. This makes Ozma a thermal and
power management system — not just monitoring, but active control with
full knowledge of the system's physical structure.

#### Fan curves

A fan curve maps a sensor reading to a fan speed. Existing tools use
sensor → fan (one-to-one). Ozma's model uses the thermal topology:
a zone has multiple sensors and multiple fans, and the curve considers
all of them.

```yaml
FanCurve:
  id: string
  name: string                  # "Silent", "Balanced", "Performance", "Full Speed"
  zone: string?                 # thermal zone this curve applies to (null = per-fan override)
  fan_ids: string[]?            # specific fans (null = all fans in zone)
  sensor_ids: string[]          # which sensors drive this curve
  sensor_mode: string           # "max" (hottest sensor wins), "average", "weighted"
  points: FanCurvePoint[]       # temperature → speed mapping
  hysteresis_c: float?          # temperature hysteresis to prevent oscillation (default: 2°C)
  ramp_rate: float?             # max speed change per second (% per sec, prevents sudden jumps)
  min_duty_percent: float?      # minimum fan speed (never stop, or allow zero-RPM)
  max_duty_percent: float?      # maximum fan speed (cap below 100% for noise)
  critical_override: CriticalOverride?  # override to full speed at critical temp

FanCurvePoint:
  temp_c: float                 # temperature threshold
  duty_percent: float           # fan duty cycle (0–100)
  # Points are interpolated linearly between them.
  # Example: [{20, 25%}, {40, 35%}, {60, 60%}, {75, 100%}]

CriticalOverride:
  threshold_c: float            # above this: override to max regardless of curve
  shutdown_c: float?            # above this: emergency shutdown
  action: string                # "max_speed", "throttle_and_max", "shutdown"
```

**What Ozma does differently from BIOS fan curves**:

| Feature | BIOS / fancontrol | Ozma |
|---------|------------------|------|
| Sensor source | One sensor per fan | Multiple sensors per zone, weighted |
| Cause awareness | No — just sees temperature number | Knows WHY temp is rising (I/O load, GPU render, ambient change) |
| Cross-zone coordination | No — each fan independent | Zone-aware — drive cage fans coordinate with exhaust fans |
| Predictive | No — reactive only | Predictive — "encode job starting, pre-ramp fans" |
| Profile switching | Manual (BIOS, software) | Automatic via intent bindings — gaming = performance, idle = silent |
| Noise-aware | No | Links fan noise to room acoustics model — adjusts during recording |
| Remote control | No | API-driven, fleet-wide, per-zone |
| Redundancy-aware | No | Knows N+1 status — compensates for failed fan |
| Power-aware | No | Knows fan power draw, adjusts within power budget |

**Intent-driven fan profiles**: Fan curves are tied to intents (§3). When
the scenario switches to `gaming`, the fan profile switches to
"Performance". When idle, "Silent". When recording audio (`creative`
intent), fans are capped to keep noise below a threshold — the system
knows the microphone's distance from each fan and the fan's noise output
at each speed.

#### Power profiles

Power profiles control CPU/GPU frequency, voltage, and power limits. Like
fan curves, these are currently managed by BIOS or per-application tools
(Ryzen Master, Intel XTU, NVIDIA GPU Tweak). Ozma unifies them with the
same intent-driven model.

```yaml
PowerProfile:
  id: string
  name: string                  # "Power Saver", "Balanced", "Performance",
                                # "Max Performance", "Silent"
  cpu: CpuPowerConfig?
  gpu: GpuPowerConfig[]?        # per-GPU
  platform: PlatformPowerConfig?

CpuPowerConfig:
  governor: string?             # "performance", "powersave", "schedutil", "conservative"
  min_freq_mhz: uint?           # minimum CPU frequency
  max_freq_mhz: uint?           # maximum CPU frequency (cap below max for power/heat)
  tdp_limit_w: float?           # PL1 / PPT power limit
  boost_limit_w: float?         # PL2 / PBO boost power limit
  boost_enabled: bool?          # allow turbo/boost clocks
  core_parking: bool?           # park idle cores (Windows)
  epp: string?                  # Energy Performance Preference (intel)
                                # "performance", "balance_performance",
                                # "balance_power", "power"
  amd_pbo: PboConfig?           # AMD Precision Boost Overdrive settings
  undervolt_mv: int?            # undervolt offset (negative = less voltage = less heat)

PboConfig:
  mode: string                  # "disabled", "enabled", "advanced"
  ppt_limit_w: float?           # Package Power Tracking limit
  tdc_limit_a: float?           # Thermal Design Current limit
  edc_limit_a: float?           # Electrical Design Current limit
  curve_optimizer: int?         # per-core curve optimizer offset (negative = undervolt)
  max_boost_override_mhz: int?  # additional boost clock offset

GpuPowerConfig:
  device_id: string             # which GPU
  power_limit_w: float?         # GPU power limit (percentage or absolute)
  core_clock_offset_mhz: int?   # core clock offset
  memory_clock_offset_mhz: int? # memory clock offset
  fan_curve: FanCurve?          # GPU-specific fan curve (overrides zone default)
  performance_mode: string?     # "max_performance", "balanced", "quiet",
                                # "power_saver" (vendor-specific)

PlatformPowerConfig:
  usb_suspend: bool?            # USB selective suspend (saves power, can cause device issues)
  pcie_aspm: string?            # "disabled", "l0s", "l1", "l0s_l1" — PCIe power saving
  sata_alpm: string?            # "disabled", "min_power", "medium_power", "max_performance"
  display_sleep_min: uint?      # display sleep timeout
  disk_sleep_min: uint?         # disk spindown timeout
  wake_on_lan: bool?            # WoL (must stay enabled for Ozma remote wake)
```

**Power profile selection via intent**:

```yaml
# Intent → power profile mapping (configurable per scenario)
IntentPowerMapping:
  gaming:       "Performance"     # max clocks, boost enabled, fans aggressive
  creative:     "Balanced"        # good performance, moderate noise
  desktop:      "Balanced"
  fidelity_audio: "Silent"       # cap CPU, cap fans, minimum noise for recording
  observe:      "Power Saver"    # minimal power when just monitoring
  control:      "Power Saver"    # headless — no display, minimal clocks
  preview:      "Power Saver"
```

#### Thermal-aware routing

The routing graph uses thermal data as an input to routing decisions:

1. **Encode job placement**: GPU at 85°C and throttling → route encode to
   iGPU (Quick Sync) or CPU (software) instead. The router checks thermal
   headroom before placing compute-intensive pipeline operations.

2. **Storage path selection**: NVMe at 70°C and throttling → if the system
   has a second NVMe or SATA drive with thermal headroom, route recording
   there instead.

3. **Predictive fan ramp**: Intent binding detects "game launching" →
   pre-ramp fans before the GPU load arrives. The system doesn't wait for
   temperature to rise — it knows the thermal consequence of the workload
   and acts proactively.

4. **Noise-sensitive scenarios**: `creative` or `fidelity_audio` intent →
   cap fans to keep ambient noise below microphone sensitivity threshold.
   The system knows: mic sensitivity (from AudioSpec), mic distance from
   each fan (from PhysicalLocation), fan noise at each speed (from FanSpec).
   It computes the maximum fan speed that keeps fan noise below the mic's
   self-noise floor at the mic's position.

5. **Power budget enforcement**: PSU at 90% capacity → reduce GPU power
   limit to create headroom. Or: UPS on battery → switch to "Power Saver"
   profile to extend runtime.

#### Observability

```
GET /api/v1/thermal/zones                   # all thermal zones with current temps + fan speeds
GET /api/v1/thermal/zones/{id}              # zone detail with sensor history
GET /api/v1/thermal/fans                    # all fans with RPM, duty, power draw
GET /api/v1/thermal/profiles                # available fan profiles
PUT /api/v1/thermal/profiles/{id}           # create/update fan profile
POST /api/v1/thermal/profiles/{id}/activate # switch active profile
GET /api/v1/power/profiles                  # available power profiles
PUT /api/v1/power/profiles/{id}             # create/update power profile
POST /api/v1/power/profiles/{id}/activate   # switch active profile
GET /api/v1/power/state                     # current CPU/GPU clocks, voltages, power draw
```

**Events**:

```
thermal.zone.warning             # zone temperature approaching threshold
thermal.zone.critical            # zone at critical temperature
thermal.zone.recovered           # zone returned to normal
thermal.fan.failed               # fan RPM dropped to zero or below minimum
thermal.fan.degraded             # fan not reaching target speed
thermal.profile.switched         # fan/power profile changed (by intent or user)
power.profile.switched           # power profile changed
power.throttle.active            # CPU or GPU thermal throttling detected
power.throttle.cleared           # throttling ended
```

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

### 5.2 Quality metadata and data freshness

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
  refresh_class: string?        # which refresh schedule this value follows (see below)
```

**Data freshness is a first-class concern.** Different data has different
natural refresh cadences, different collection costs, and different staleness
tolerances. The system must be explicit about all three — consumers need to
know not just *when* data was collected, but *why* it's that old and *when*
it will next be refreshed.

**RefreshSchedule** — global primitive defining how often each class of
data is collected, and what it costs to collect it:

```yaml
RefreshSchedule:
  classes: RefreshClass[]

RefreshClass:
  id: string                    # class identifier (referenced by QualifiedValue.refresh_class)
  name: string                  # human-readable
  default_interval_s: float     # how often to refresh by default
  min_interval_s: float?        # fastest allowed refresh (protects against overload)
  adaptive: bool                # does the interval adjust based on conditions?
  cost: RefreshCost             # what it costs to collect this data
  staleness: StalenessPolicy    # how to handle aged-out data
  trigger_refresh_on: string[]? # events that trigger an immediate refresh outside the schedule

RefreshCost:
  cpu_impact: string            # "negligible", "low", "moderate", "high"
  io_impact: string             # "none", "disk_read", "disk_write", "network"
  device_impact: string         # "none", "wake_disk", "interrupt_device", "bus_contention"
  duration_ms: float?           # typical collection time
  notes: string?                # why this cost exists

StalenessPolicy:
  fresh_threshold_s: float      # below this age: data is fresh, full trust
  stale_threshold_s: float      # above this age: data is stale, reduced trust
  expired_threshold_s: float?   # above this age: data is expired, flag prominently
  action_on_stale: string       # "degrade_quality" (reduce InfoQuality),
                                # "flag_only" (mark stale but keep quality),
                                # "trigger_refresh" (attempt immediate refresh)
  action_on_expired: string     # "degrade_quality", "flag_only", "remove_from_graph"
```

**Standard refresh classes**:

| Class | Default interval | Cost | Staleness | Rationale |
|-------|-----------------|------|-----------|-----------|
| `realtime_metrics` | 1–5s | Negligible (read /proc, sysfs) | Stale at 15s, expired at 60s | CPU %, memory, temperature, fan RPM — cheap to read, changes constantly |
| `network_health` | 5–10s | Low (ping, packet counters) | Stale at 30s, expired at 120s | Latency, jitter, loss — needs periodic probing |
| `link_bandwidth` | 30–60s | Moderate (passive measurement) | Stale at 5min, expired at 30min | Available bandwidth — measured from traffic flow |
| `usb_topology` | On event + 5min poll | Moderate (lsusb -t, udevadm) | Stale at 10min, expired at 1h | USB tree — changes on hotplug, otherwise static |
| `pcie_topology` | On boot + 1h poll | Low (lspci, sysfs) | Stale at 2h, expired at 24h | PCIe devices — rarely change at runtime |
| `smart_health` | 1–24h | Moderate (disk I/O, can wake spun-down disks) | Stale at 48h, expired at 7d | Drive health — querying wakes sleeping disks |
| `sfp_dom` | 30s | Low (I2C read from module) | Stale at 2min, expired at 10min | Optical power, temperature — cheap, changes with conditions |
| `firmware_versions` | 24h + on boot | Low (fwupd query, dmidecode) | Stale at 48h, expired at 7d | Changes only on update — no reason to poll frequently |
| `power_rails` | 1–5s (if INA219) | Negligible (I2C read) | Stale at 15s, expired at 60s | Voltage/current — changes with load |
| `power_rails` | 60s (if inferred) | Low (read voltage, compute) | Stale at 5min, expired at 30min | Inferred current — less time-sensitive |
| `pdu_metering` | 10–60s | Low (SNMP poll) | Stale at 2min, expired at 10min | Per-outlet power — via SNMP/vendor API |
| `ups_state` | 10–30s | Low (NUT query) | Stale at 60s, expired at 5min | Battery state — critical during outage |
| `bios_version` | On boot only | Negligible (dmidecode) | Never stale (doesn't change at runtime) | Static until reboot after flash |
| `device_db_match` | On first discovery | Negligible (local lookup) | Never stale (cached) | Device identification — doesn't change |
| `bluetooth_connection` | 5–10s | Low (bluetoothctl, D-Bus) | Stale at 30s, expired at 2min | RSSI, codec, battery — changes with distance |
| `wifi_signal` | 5–10s | Low (iw, nl80211) | Stale at 30s, expired at 2min | RSSI, channel utilisation — changes constantly |
| `thermal_zone` | 1–5s | Negligible (sysfs) | Stale at 15s, expired at 60s | Zone temperatures — critical for fan control |
| `room_occupancy` | 10–60s | Varies (camera inference: high; PIR: negligible) | Stale at 5min, expired at 30min | Presence detection — impacts intent bindings |

**Adaptive refresh**: When `adaptive: true`, the refresh interval adjusts
based on conditions:

- **Under pressure**: If a device is near a thermal/power/resource limit,
  its metrics refresh faster (5s → 1s) for tighter control.
- **Idle**: If nothing is changing, refresh slows down (1s → 10s) to reduce
  CPU overhead and I/O.
- **Active pipeline**: Devices in an active pipeline refresh their relevant
  metrics faster than idle devices.
- **On battery**: Agent on a laptop reduces refresh rates to save power.
- **Constrained device**: A node with limited CPU (Pi Zero) uses longer
  intervals than a full controller.

The refresh schedule is **configurable per device, per class**. A user who
cares about thermal monitoring can set `thermal_zone` to 1s refresh on
their overclocked server. A user on a Pi Zero can set `usb_topology` to
30-minute polling. The defaults are sensible for typical hardware.

**Device-level freshness**: Every device in the graph carries a summary of
its data freshness:

```yaml
DeviceFreshness:
  online: bool                  # is this device currently reachable?
  last_contact: timestamp       # when we last heard from this device
  last_full_refresh: timestamp  # when all data classes were last refreshed
  per_class: ClassFreshness[]   # per-refresh-class status

ClassFreshness:
  class: string                 # refresh class ID
  last_refreshed: timestamp     # when this class was last refreshed
  next_refresh: timestamp?      # when the next refresh is scheduled
  age_s: float                  # current age in seconds
  state: string                 # "fresh", "stale", "expired", "never_collected"
  error: string?                # if last refresh failed: why
```

This sits on the Device primitive alongside `capacity`, `power_profile`,
and `version`. API consumers can check: "Is this device's thermal data
fresh? When will it next be updated? Is the SMART data expired because the
drive is sleeping and we don't want to wake it?"

**Display in dashboards and API**: Every value returned by the API includes
its age. Stale values are visually flagged. Expired values are prominently
marked. A device that's been offline for 2 hours shows all its data with
"last updated 2h ago" — the data is still useful (the device's serial
number hasn't changed, its chipset topology hasn't changed, its last-known
thermal state is informative) but consumers know to treat dynamic values
(temperature, bandwidth, power) with appropriate scepticism.

### 5.3 Quality decay

Quality decay is the automatic mechanism that adjusts `InfoQuality` based
on the age of the data, using the staleness policy from the value's
refresh class:

```
effective_quality = base_quality                if age < fresh_threshold
effective_quality = degrade(base_quality)       if age > stale_threshold
effective_quality = degrade(base_quality, 2)    if age > expired_threshold
```

The decay function reduces quality by one level per threshold crossing:
`measured` → `reported` (stale), `reported` → `assumed` (expired). This
means routing decisions automatically become more conservative as data
ages — the router increases safety margins on stale values (§5.4).

Re-measurement is triggered proactively for stale values on active
pipelines — if a link in a live video pipeline has stale bandwidth data,
the transport plugin is asked to re-measure before the data expires.

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
| `firewire` | IEEE 1394a/b. Isochronous audio (guaranteed bandwidth, daisy-chainable). |

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
# Includes: serial numbers, MAC addresses, firmware versions, purchase/first-seen
# date, physical location, current health, connected topology.
# Exportable as CSV/JSON for compliance evidence, insurance, auditing.

# Asset lifecycle
GET /api/v1/monitoring/inventory/{device_id}/history
# Returns: complete lifecycle for one device — first seen, every firmware update,
# every location change, every state change, health trend, connected-to history.

# Asset search
GET /api/v1/monitoring/inventory/search?serial=ABC123
GET /api/v1/monitoring/inventory/search?mac=AA:BB:CC:DD:EE:FF
GET /api/v1/monitoring/inventory/search?location=rack-1
GET /api/v1/monitoring/inventory/search?status=degraded
# Search across all asset fields — serial, MAC, location, type, status, vendor.

# Sankey / flow diagram data for any resource type
GET /api/v1/monitoring/sankey?type=bandwidth
GET /api/v1/monitoring/sankey?type=power
GET /api/v1/monitoring/sankey?type=audio
GET /api/v1/monitoring/sankey?type=storage_io
GET /api/v1/monitoring/sankey?device_id={id}&type=bandwidth
# Returns: directed graph of flows with source, destination, and magnitude
# at each hop. Renderable as a Sankey diagram, flow map, or treemap.
# type=bandwidth: bytes/sec through every link, showing shared segments and bottlenecks
# type=power: watts from PSU rails through every device to every consumer
# type=audio: audio signal paths with gain, format, and latency per hop
# type=storage_io: I/O flow from applications through controllers to drives
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
- **Asset lifecycle tracking** — every device has a complete history from
  first discovery to current state: when it was first seen (acquisition/
  deployment), every firmware update, every physical location change, every
  health state transition, every connection change. This is IT asset
  management (ITAM) built into the routing graph — no separate CMDB or
  spreadsheet. The journal + hardware identity + device database entry
  together give you: what it is, which specific one, where it is, what
  state it's in, and everything that ever happened to it.
- **Internal hardware tracking** — for Ozma Labs and any organisation: track
  provenance and status of every piece of hardware from acquisition through
  deployment to disposal. Know which dev board ran which firmware, which
  capture card has had USB errors, which SFP module was reflashed, which
  PSU cable belongs to which PSU. The state change journal is the source
  of truth — queryable, exportable, auditable.

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
  # --- Ozma software version (for Ozma-managed components) ---
  component: string?            # "controller", "node", "agent", "softnode",
                                # "screen_firmware", "rgb_firmware", "plugin"
  current_version: SemVer?      # currently running version
  channel: string?              # "stable", "beta", "nightly", "pinned"
  platform: string?             # "linux-amd64", "linux-arm64", "linux-riscv64",
                                # "windows-amd64", "macos-arm64", "esp32", "rp2040"
  build_info: BuildInfo?        # build metadata
  update_state: UpdateState     # current update status
  protocol_version: string?     # ozma protocol version this device speaks ("ozma/0.1")
  min_compatible: string?       # minimum controller version this device works with
  max_compatible: string?       # maximum controller version (if known)

  # --- Third-party device firmware (for any device with updateable firmware) ---
  firmware: FirmwareInfo[]?     # firmware components on this device (may be multiple)

FirmwareInfo:
  component: string             # what firmware this is
  current_version: string?      # running firmware version (as reported by device)
  vendor: string?               # firmware vendor
  device_type: string?          # LVFS device type / GUID
  updatable: bool               # can this firmware be updated?
  update_method: FirmwareUpdateMethod?  # how to update it
  update_state: UpdateState     # current update status
  known_issues: FirmwareIssue[]?  # known bugs in current version
  history: FirmwareVersion[]?   # version history (from device database or LVFS)

FirmwareUpdateMethod:
  mechanism: string             # "fwupd", "vendor_tool", "usb_dfu", "ota_http",
                                # "via_qmk", "bluetooth_ota", "serial_flash",
                                # "bios_flash", "manual_rom", "not_updatable"
  lvfs: bool                    # available via LVFS (Linux Vendor Firmware Service)
  lvfs_guid: string?            # LVFS device GUID for update matching
  vendor_url: string?           # vendor's firmware download page
  requires: string?             # special requirements ("windows_only", "vendor_app",
                                # "usb_cable", "bluetooth", "bios_menu")
  risk: string                  # "safe", "low", "medium", "high", "brick_risk"
  # "safe" = automatic, reliable rollback (fwupd with capsule update)
  # "low" = well-tested process, rare failures
  # "medium" = vendor tool required, some failure reports
  # "high" = manual process, failure = RMA or recovery mode
  # "brick_risk" = no recovery if interrupted (some keyboard MCUs, old SSDs)

FirmwareVersion:
  version: string
  date: string?                 # release date
  changelog: string[]?          # what changed
  known_issues: FirmwareIssue[]?
  known_fixes: string[]?        # issues fixed in this version
  lvfs_release: string?         # LVFS release ID (if available)
  source: string?               # "lvfs", "vendor", "community"

FirmwareIssue:
  id: string
  severity: string              # "critical", "major", "minor", "cosmetic"
  category: string              # subsystem affected
  summary: string
  description: string?
  workaround: string?
  fixed_in: string?             # firmware version that fixes it
  cve: string?                  # CVE if security-related
  affects_ozma: bool?
  ozma_impact: string?

BuildInfo:
  commit: string?               # git commit hash
  build_date: timestamp?        # when this build was produced
  edition: string?              # "open_source", "free", "commercial"
  signature: string?            # Ed25519 signature (base64)
  signed_by: string?            # signing key identifier

UpdateState:
  status: up_to_date | update_available | updating | update_failed | unknown
  available_version: string?    # newest version available (null if up_to_date or unknown)
  available_channel: string?    # channel of available update
  last_check: timestamp?        # when we last checked for updates
  last_update: timestamp?       # when the device was last updated
  failure_reason: string?       # if update_failed, why
  can_update: bool              # does this device support remote update?
  requires_reboot: bool?        # will the update require a restart?
  rollback_available: bool?     # can this device roll back to previous version?
```

**What has firmware** — almost everything:

| Device class | Firmware components | Update method | LVFS coverage |
|-------------|-------------------|---------------|---------------|
| Motherboard | BIOS/UEFI (see §15 BiosDatabase) | BIOS flash, fwupd capsule | Good (many vendors) |
| CPU | Microcode | Loaded by OS or BIOS | Via BIOS update |
| GPU | VBIOS, driver firmware | Vendor tool, fwupd | Partial (some NVIDIA/AMD) |
| SSD/NVMe | Controller firmware | fwupd, vendor tool | Good (Samsung, WD, Intel, Crucial) |
| HDD | Controller firmware | Vendor tool | Limited |
| Thunderbolt dock | Thunderbolt controller FW, USB hub FW, PD controller FW | fwupd, vendor tool | Good (CalDigit, Lenovo, Dell) |
| USB hub | Hub controller firmware | Vendor tool (rare) | Rare |
| Monitor | Scaler firmware | OSD menu, USB, vendor app | Rare (Dell via fwupd) |
| Keyboard (QMK/VIA) | MCU firmware | QMK DFU, VIA | No (community tooling) |
| Keyboard (vendor) | MCU firmware | Vendor app (iCUE, Synapse, G Hub) | No |
| Mouse | Sensor + wireless firmware | Vendor app | No |
| Headset | DSP firmware, Bluetooth firmware | Vendor app | Rare |
| Webcam | ISP firmware | fwupd, vendor tool | Partial (Logitech) |
| Network card | NIC firmware | fwupd, ethtool flash | Good (Intel, Mellanox) |
| WiFi card | WiFi firmware | fwupd, kernel firmware | Good (Intel) |
| Bluetooth adapter | BT firmware | fwupd, kernel firmware | Partial |
| WLED controller | ESP firmware | OTA HTTP | No (Ozma manages directly) |
| Stream Deck | MCU firmware | Elgato app, fwupd | Partial |
| Printer | Controller firmware | Vendor tool | Partial (HP, Lexmark) |
| UPS | Controller firmware | NUT, vendor tool | Rare |
| Smart PSU | Controller firmware | Vendor app (Corsair Link, etc.) | No |
| USB-C PD controller | PD firmware | fwupd | Partial (TI, Cypress/Infineon) |

**LVFS / fwupd integration**:

LVFS (Linux Vendor Firmware Service) is the primary data source for
third-party firmware versions and updates. `fwupd` is the client that
reads from LVFS and applies updates.

```yaml
LvfsIntegration:
  # The controller/agent queries fwupd for firmware state of all devices
  discovery: string             # "fwupdmgr get-devices" — lists all fwupd-visible devices
                                # with current version, update availability, and GUID
  update_check: string          # "fwupdmgr get-updates" — available updates from LVFS
  history: string               # "fwupdmgr get-history" — past update attempts + results

  # Mapping to the routing graph:
  # For each fwupd device:
  #   1. Match to graph device via USB VID/PID, PCI ID, or device path
  #   2. Populate FirmwareInfo.current_version from fwupd
  #   3. Populate FirmwareInfo.update_state from LVFS metadata
  #   4. Populate FirmwareInfo.known_issues from device database
  #   5. Cross-reference with device database for Ozma-specific impact
```

On Linux, fwupd is the primary mechanism. On Windows, the agent queries
Windows Update for driver/firmware versions and checks vendor APIs where
available. On macOS, `system_profiler` provides firmware versions for
Apple hardware and some third-party devices.

**Firmware as a routing concern**:

Firmware versions affect device capabilities and reliability. The routing
graph needs to know about firmware because:

1. **Known bugs affect routing quality**: A Thunderbolt dock with firmware
   v1.2 has a known USB dropout bug. The router marks the dock's links as
   lower reliability until firmware is updated.

2. **New firmware enables new capabilities**: A monitor firmware update adds
   VRR support. A NIC firmware update enables 2.5GbE on hardware that
   previously only ran at 1GbE. The device database tracks which capabilities
   require which firmware version.

3. **Security vulnerabilities**: Firmware CVEs (Thunderbolt Spy, SSD
   encryption bypass, NIC remote code execution) should be surfaced. The
   agent detects the firmware version, the database flags the CVE, the
   dashboard shows the warning.

4. **Fleet firmware management**: For managed deployments, firmware versions
   across all devices in the fleet should be visible. "3 of 8 docks are
   running firmware with a known USB bug — update them." This is the same
   fleet version management as §14.3 but for third-party devices.

**Firmware observability**:

```
GET /api/v1/firmware/devices             # all devices with firmware info
GET /api/v1/firmware/devices/{id}        # firmware detail for one device
GET /api/v1/firmware/updates-available   # all devices with pending firmware updates
GET /api/v1/firmware/issues              # all known firmware issues affecting current fleet
GET /api/v1/firmware/history             # firmware update history
POST /api/v1/firmware/check              # trigger firmware update check (fwupd refresh)
POST /api/v1/firmware/update/{id}        # apply firmware update (if safe + user approves)
```

**Events**:

```
firmware.update_available       # new firmware detected for a device
firmware.update_applied         # firmware successfully updated
firmware.update_failed          # firmware update failed
firmware.issue_detected         # known issue affects a device's current firmware
firmware.security_advisory      # CVE affects a device's firmware
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
  # PC platform
  motherboard: MotherboardSpec? # chipset, CPU socket, internal topology, physical port map
  cpu: CpuSpec?                 # CPU/SoC capabilities, cache, iGPU, memory controller
  chipset: ChipsetSpec?         # PCH/southbridge: what connects where, lane allocation
  ram: RamSpec?                 # DIMM modules: speed, timings, XMP/EXPO, capacity, RGB
  gpu: GpuSpec?                 # GPUs (encode/decode capabilities, display outputs, display engine)
  storage: StorageSpec?         # SSDs, HDDs, NVMe drives
  psu: PsuSpec?                 # power supply: wattage, rails, efficiency, modularity
  cooler: CoolerSpec?           # CPU cooler, AIO, custom loop components
  case_component: CaseSpec?     # PC case (fan slots, drive bays, radiator support)
  pcie: PcieCardSpec?           # PCIe add-in cards (NICs, capture cards, USB controllers, sound cards)
  laptop: LaptopSpec?           # laptop-specific: GPU switching, MUX, power states, thermal modes
  cable: CableSpec?             # cables that affect signal quality (HDMI, DP, USB, Ethernet)
  adapter: AdapterSpec?         # adapters, risers, converters that bridge between interface types

  # Peripherals and input
  keyboard: KeyboardSpec?
  mouse: MouseSpec?
  audio: AudioSpec?             # microphones, speakers, headphones, audio interfaces
  display: DisplaySpec?         # monitors, projectors (compound device model)
  capture: CaptureSpec?         # capture cards (USB, PCIe, Thunderbolt)
  camera: CameraSpec?
  control: ControlSpec?         # control surface capabilities
  hub: HubSpec?
  dock: DockSpec?

  # Infrastructure
  network: NetworkCardSpec?     # NICs, WiFi cards, Bluetooth adapters
  network_switch: NetworkSwitchSpec?
  router: RouterSpec?
  access_point: AccessPointSpec?
  avr: AvrSpec?                 # AV receiver (HDMI switch + audio processor + amplifier)
  audio_interface: AudioInterfaceSpec?  # pro audio interface (multi-I/O, DSP, routing matrix)
  power: PowerSpec?             # generic power (PSU for non-PC devices, PoE injectors, etc.)
  power_distribution: PowerDistributionSpec?  # PDU, UPS, power strip, surge protector, extension
  transceiver: TransceiverSpec?   # SFP/SFP+/SFP28/QSFP+/QSFP28/QSFP-DD modules
  fiber_cable: FiberCableSpec?    # fiber optic cables (single-mode, multi-mode, OM1-OM5)
  server_chassis: ServerChassisSpec?  # server chassis (1U-4U, blade, multi-node)
  rack: RackSpec?                 # server/network racks (full-depth, half-depth, open frame, LackRack)
  rack_accessory: RackAccessorySpec?  # patch panels, shelves, drawers, blanking panels, cable management
  sensor: SensorSpec?
  actuator: ActuatorSpec?

  # Environment
  furniture: FurnitureSpec?     # desks, racks, mounts — physical dimensions + slots + state
  rgb: RgbSpec?                 # LED layout, zones (see device-db.md for full spatial schema)
  screen: ScreenSpec?           # embedded screen capabilities

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
                                # "nvme", "sata", "thunderbolt", "sound_card", "fpga",
                                # "firewire_controller"]
```

A PCIe card is often compound — a GPU has display outputs, encode/decode
engines, and sometimes a USB-C port with DP alt mode. A Thunderbolt add-in
card has a USB controller, a DP input, and a Thunderbolt port. The `subsystem`
field lists what the card provides, and the detailed capabilities live in
the relevant category blocks (`gpu`, `capture`, `hub`, etc.) on the same entry.

**FireWireControllerSpec** (IEEE 1394 host controllers — chipset matters
enormously for pro audio compatibility):

```yaml
FireWireControllerSpec:
  standard: string              # "1394a" (FireWire 400), "1394b" (FireWire 800),
                                # "1394b_s3200" (3.2 Gbps, rare)
  chipset: string?              # CRITICAL for pro audio compatibility
  chipset_vendor: string?       # "ti" (Texas Instruments), "via", "agere", "lsi",
                                # "nec_renesas", "ricoh", "jmicron", "oxford"
  ports: FireWirePort[]
  bus: FireWireBusSpec

FireWirePort:
  id: string                    # "fw_1", "fw_2"
  connector: string             # "firewire_400" (6-pin or 4-pin), "firewire_800" (9-pin)
  powered: bool?                # 6-pin/9-pin = bus-powered, 4-pin = unpowered
  max_power_ma: uint?           # bus power per port (typically 1500mA at 12V for 6-pin)
  physical: PhysicalPortInfo?

FireWireBusSpec:
  max_speed_mbps: uint          # 400 (S400), 800 (S800), 1600 (S1600), 3200 (S3200)
  isochronous_channels: uint?   # guaranteed-bandwidth channels (typically 64)
  isochronous_bandwidth_percent: float?  # max bandwidth allocatable to isochronous (80%)
  # Isochronous is what makes FireWire good for audio — guaranteed bandwidth
  # with bounded latency, unlike USB's best-effort polling. One 96kHz/24-bit
  # 8-channel ADAT stream over FireWire uses one isochronous channel with
  # guaranteed delivery.
  max_devices: uint?            # max devices on one bus (63)
  max_hops: uint?               # max cable hops in daisy chain (16 for 1394a, 63 for 1394b)
  bus_topology: string          # "daisy_chain", "tree", "point_to_point"
```

**Why chipset matters**:

| Chipset | Pro audio compatibility | Notes |
|---------|----------------------|-------|
| TI (Texas Instruments) XIO2213, TSB43AB23 | **Excellent** — the gold standard | Every pro audio vendor recommends TI. Stable isochronous timing. |
| TI TSB82AA2 | Excellent | Newer TI, FireWire 800. Recommended. |
| Agere FW643 | Good | Used in Apple Macs. Compatible with most interfaces. |
| LSI FW643E | Good | Same silicon as Agere (LSI acquired Agere). |
| VIA VT6306, VT6307 | **Poor** — known issues | Isochronous timing errors. Audio dropouts with many devices. Budget cards use these. |
| NEC/Renesas μPD72873, μPD72874 | Fair | Works for simple setups. Struggles with high channel counts. |
| Ricoh R5C832 | Fair | Found in laptops. Some interfaces won't initialise. |
| JMicron JMB38x | **Poor** — avoid for audio | Same issues as VIA. Not recommended by any audio vendor. |
| Oxford OXFW971 | Good | Used in some FireWire-to-PCIe bridges. |

This is knowledge that exists in forum posts and vendor FAQs but nowhere
in a structured database. The device database captures it — when a user
adds a FireWire audio interface and the agent detects a VIA chipset
FireWire controller: "Your FireWire controller uses a VIA VT6307 chipset.
This is known to cause audio dropouts with pro audio interfaces. Consider
a TI-based FireWire PCIe card (e.g., Syba SD-PEX30009) for reliable
operation."

**FireWire topology in the graph**:

FireWire is a bus with daisy-chain or tree topology — fundamentally
different from USB's host-centric star. Multiple devices share the bus
bandwidth, and isochronous channels provide guaranteed-bandwidth reservations.

```
FireWire bus (800 Mbps)
├── Host controller (TI TSB82AA2, PCIe x1)
│   └── Port 1 (9-pin FireWire 800) → Cable →
│       └── RME Fireface 800
│           ├── Uses: 2 isochronous channels (in + out), ~40 Mbps
│           └── Port (9-pin FW800) → Cable →
│               └── MOTU 828mk3
│                   ├── Uses: 2 isochronous channels, ~40 Mbps
│                   └── Port (9-pin FW800) → [end of chain]
│
│   Total bus: 800 Mbps, ~80 Mbps used, 80% iso budget = 640 Mbps max
│   Headroom: plenty for 2 interfaces
```

The routing graph models each device on the FireWire bus as a device with
a `firewire` transport link. The bus is a shared medium (like RS-485 in
§ serial) — bandwidth is shared, isochronous channels are reserved.
Adding a third interface may exceed the isochronous budget. The router
checks: "Bus has 640 Mbps isochronous budget. Currently using 80 Mbps.
Adding a third 40-channel interface (160 Mbps) would bring total to
240 Mbps — within budget."

**FireWire as a built-in transport**:

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

**USB displays and software-rendered outputs**:

USB display adapters (DisplayLink, Fresco Logic) and virtual displays
(macOS Sidecar, Windows Miracast, headless display emulators) bypass the
GPU's display engine entirely. They don't consume heads, PLLs, or output
links — they render via the CPU or GPU's 3D engine, compress the
framebuffer, and send it over USB or network. This makes them a workaround
for head-limited systems (like Apple M1) but with different tradeoffs.

```yaml
UsbDisplaySpec:
  type: string                  # "displaylink", "fresco_logic", "mcdp2900",
                                # "evdi" (virtual), "sidecar", "miracast",
                                # "headless_dongle"
  chipset: string?              # "DL-6950", "FL2000", "MCDP2900"
  max_resolution: Resolution?
  max_refresh: float?
  compression: string?          # "displaylink_adaptive", "h264", "h265", "none"
  connection: string            # "usb3", "usb2", "wifi", "thunderbolt", "virtual"

  # Performance characteristics (very different from native display output)
  cpu_usage_percent: float?     # CPU overhead for rendering + compression
  gpu_usage_percent: float?     # GPU overhead (some use GPU for encode)
  latency_ms: float?            # additional display latency vs native output
  color_accuracy: string?       # "full" (no compression artefacts),
                                # "good" (slight compression), "limited"
  hdr_supported: bool?          # typically no for USB displays

  # Does NOT consume a display engine head/PLL/link.
  # This is the key property — it's an additional display path
  # independent of the GPU's native output constraints.
  consumes_head: bool           # always false for USB displays
  consumes_encode_session: bool? # true if uses GPU video encoder for compression
```

**Display path types** — a unified model of how displays are driven:

| Path type | Heads used | Latency | Quality | CPU/GPU cost | Use case |
|-----------|-----------|---------|---------|-------------|----------|
| Native (DP/HDMI) | Yes — 1 head + PLL + output link | <1ms | Perfect | None | Primary displays |
| DP Alt Mode (USB-C) | Yes — same as native DP | <1ms | Perfect | None | Laptop/dock displays |
| Thunderbolt DP tunnel | Yes — head used, DP tunneled | 1–2ms | Perfect | Minimal | TB dock displays |
| DisplayLink USB | No | 5–30ms | Good (compressed) | 5–15% CPU | Extra monitors, M1 workaround |
| EVDI / virtual | No | 1–5ms | Perfect (uncompressed) | 5–10% CPU | Headless servers, testing |
| Sidecar (macOS) | No | 10–30ms | Good | 5–10% CPU + GPU | iPad as display |
| Miracast | No | 20–50ms | Good (H.264) | 5–10% GPU encode | Wireless display |
| Headless dongle | Yes — consumes 1 head | <1ms | N/A (no physical display) | None | Tricks GPU into rendering for capture |

**Apple silicon display limits** — the canonical example of head constraints
where USB displays are the workaround:

```yaml
# Apple M1 display engine
display_engine:
  heads: 2                      # M1 has exactly 2 display heads
  max_external: 1               # only 1 external display via native output
  # The internal panel uses 1 head. That leaves 1 head for external.
  # This is a hard limit — no amount of adapters changes it.
  output_links:
    - { output_id: internal, link_type: edp, head_assignment: "head_0" }
    - { output_id: thunderbolt_0, link_type: dp_tunnel, head_assignment: "head_1" }
    - { output_id: thunderbolt_1, link_type: dp_tunnel, head_assignment: "head_1" }
    # Both TB ports share head_1 — you can use one OR the other, not both.
  constraints:
    - type: max_external
      description: "M1 supports only 1 external display via native output"
      outputs_affected: ["thunderbolt_0", "thunderbolt_1"]
    - type: shared_head
      description: "Both Thunderbolt ports share one display head — only one external via DP"

# Apple M1 Pro/Max display engine
display_engine:
  heads: 4                      # M1 Pro: 3 heads (2 external). M1 Max: 5 heads (4 external).
  max_external: 2               # M1 Pro. M1 Max = 4.
  # No single-external limit. Multiple external displays work natively.

# Apple M2 display engine
display_engine:
  heads: 2                      # same 1-external limit as M1
  max_external: 1

# Apple M3 display engine
display_engine:
  heads: 3                      # M3 supports 2 external (improvement over M1/M2)
  max_external: 2

# Apple M4 display engine
display_engine:
  heads: 4                      # M4 supports 3 external
  max_external: 3
```

The router uses this to:

1. **Warn before hitting the limit**: "Your M1 MacBook supports 1 external
   display via Thunderbolt. Adding a second monitor requires a DisplayLink
   adapter (USB display, not native — expect 5–30ms latency and CPU overhead)."

2. **Recommend the right solution**: "You need 3 displays on an M1. Options:
   (a) 1 native Thunderbolt + 2 DisplayLink USB (CPU overhead, latency).
   (b) Upgrade to M1 Pro which supports 2 native + 1 DisplayLink.
   (c) Use a DisplayLink triple-head dock (3 USB displays, 15% CPU)."

3. **Account for USB display overhead**: A DisplayLink adapter consumes
   CPU and possibly a GPU encode session. The resource model (§2.7)
   tracks this. Three DisplayLink adapters on an M1 = ~30% CPU just for
   display rendering — the router warns about resource pressure.

4. **Headless dongle awareness**: A headless HDMI/DP dongle (EDID emulator)
   consumes a real display head — it tricks the GPU into rendering a
   framebuffer for capture. On an M1, plugging in a headless dongle
   uses the only external head, preventing a real monitor. The router
   flags this: "Headless dongle on Thunderbolt is using your only
   external display head. Consider a virtual display (EVDI) instead."

**Discovery**: Display engine capabilities can come from:
- Device database entry (`spec` quality) — known constraints per GPU model
- Driver query (`reported` quality) — `nvidia-smi`, `xrandr --verbose`,
  DRM/KMS `drmModeGetResources` for head/CRTC count
- Measured (`measured` quality) — attempt a configuration, observe if it
  succeeds or the driver downclocks
- Platform APIs (`reported`) — macOS `CGGetActiveDisplayList` for current
  count, `IODisplayConnect` for output topology, `system_profiler` for
  chip identity

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
  # --- Individual drive ---
  interface: string?            # "nvme", "sata", "sas", "usb", "sd_card", "u2", "edsff"
  form_factor: string?          # "m2_2280", "m2_2242", "m2_2230",
                                # "2.5_inch", "3.5_inch", "u2_2.5", "edsff_e1s",
                                # "sd", "microsd", "msata", "pcie_aic"
  protocol: string?             # "ahci", "nvme", "scsi", "usb_mass_storage", "usb_uas"
  capacity_gb: uint?
  sequential_read_mbps: float?
  sequential_write_mbps: float?
  random_read_iops: uint?
  random_write_iops: uint?
  endurance_tbw: float?         # TBW rating
  type: string?                 # "nand_tlc", "nand_qlc", "nand_slc", "nand_mlc",
                                # "hdd_cmr", "hdd_smr", "optane"
  rpm: uint?                    # HDD rotational speed (5400, 7200, 10000, 15000)
  dram_cache: bool?
  dram_cache_mb: uint?
  power_loss_protection: bool?
  smart: SmartAttributes?       # SMART health data (if available)
  firmware: FirmwareInfo?       # drive firmware (see §14.1)
  controller: string?           # drive controller chipset ("Phison E18", "Samsung Elpis",
                                # "WD in-house", "Marvell 88SS1321")

  # --- SAS-specific ---
  sas: SasSpec?                 # SAS drive properties (if SAS interface)

  # --- Encryption ---
  hardware_encryption: string?  # "opal_2.0", "sed", "none", "bitlocker_edrive"

SmartAttributes:
  health: string?               # "healthy", "warning", "failing"
  temperature_c: float?
  power_on_hours: uint?
  reallocated_sectors: uint?
  wear_leveling_percent: float? # SSD wear (0% = new, 100% = end of life)
  media_errors: uint?           # NVMe media/data integrity errors
  unsafe_shutdowns: uint?
  # SMART data feeds into trend analysis (§11.7) — increasing reallocated
  # sectors or declining wear leveling triggers a trend alert.

SasSpec:
  sas_version: string?          # "sas_2", "sas_3", "sas_4"
  link_speed_gbps: float?       # 6, 12, 22.5
  dual_port: bool?              # SAS drives can have two ports for multipath
  sas_address: string?          # SAS WWN (World Wide Name)
```

**Storage controllers — HBAs, RAID cards, and NVMe switches**:

A storage controller (HBA or RAID card) is a compound device that connects
drives to the system bus. It has its own internal topology, firmware, cache,
and bandwidth constraints.

```yaml
StorageControllerSpec:
  controller_type: string       # "hba", "raid", "nvme_switch", "sata_controller",
                                # "usb_storage_bridge"
  interface_to_host: string     # "pcie_x8", "pcie_x4", "pcie_x16", "chipset_sata"
  host_bandwidth_gbps: float?   # total bandwidth to host
  ports: StoragePort[]          # physical connectors on the controller
  firmware: FirmwareInfo?       # controller firmware
  cache: ControllerCache?       # write cache (RAID cards)
  raid_levels: string[]?        # supported RAID levels ["0","1","5","6","10","50","60"]
  jbod: bool?                   # supports pass-through / IT mode
  max_drives: uint?             # maximum drives supported
  chipset: string?              # "Broadcom SAS3816", "LSI SAS3008", "Marvell 88SE9230",
                                # "ASMedia ASM1166", "JMicron JMB585"

StoragePort:
  id: string                    # "sas_0", "sata_0", "mini_sas_hd_0"
  connector: string             # "sata", "sas_sff8482", "mini_sas_hd_sff8643",
                                # "mini_sas_sff8087", "u2_sff8639", "oculink",
                                # "m2_m_key", "m2_b_key"
  protocol: string[]            # ["sas", "sata"] (SAS ports accept both SAS and SATA)
  link_speed_gbps: float        # per-link speed (6, 12, 22.5)
  lanes: uint                   # number of PHY lanes (SAS: 1 per drive; mini-SAS HD: 4)
  shared_bandwidth: bool        # do drives on this port share bandwidth?
  physical: PhysicalPortInfo?

ControllerCache:
  size_mb: uint?                # cache size
  type: string?                 # "write_back", "write_through", "none"
  battery_backed: bool?         # BBU or supercap protects cache on power loss
  flash_backed: bool?           # flash-backed write cache (survives extended outage)
```

**Drive enclosures and backplanes** — the 24-bay SAS case:

A drive enclosure is a compound device containing a backplane, SAS/SATA
expander(s), drive bays, and cooling. It connects to an HBA via one or more
SAS cables and presents multiple drive slots on a shared backplane.

```yaml
DriveEnclosureSpec:
  enclosure_type: string        # "das_jbod", "nas_enclosure", "server_chassis",
                                # "external_bay", "hot_swap_cage"
  bays: DriveBay[]              # physical drive bays
  backplane: BackplaneSpec      # how drives connect internally
  expanders: SasExpander[]?     # SAS expanders (if any)
  uplinks: EnclosureUplink[]    # connections to host/HBA
  cooling: EnclosureCooling?    # fans, temperature sensors
  power: EnclosurePower?        # PSU, redundancy
  management: EnclosureManagement?  # SES, SGPIO, enclosure management

DriveBay:
  id: string                    # "bay_0" through "bay_23"
  form_factor: string           # "3.5_inch", "2.5_inch", "edsff_e1s"
  hot_swap: bool
  position: PortPosition?       # physical position in the enclosure
  connected_to: string          # which expander/backplane port this bay is on
  populated: string?            # device database ID of installed drive (if known)
  activity_led: bool?           # per-bay activity LED
  fault_led: bool?              # per-bay fault LED
  power_disable: bool?          # can individual bay power be controlled?

BackplaneSpec:
  protocol: string              # "sas", "sata", "nvme", "mixed"
  ports: uint                   # total backplane ports (= max drives)
  zones: BackplaneZone[]?       # if the backplane is zoned (different zones on different expanders)

BackplaneZone:
  id: string
  bays: string[]                # which bays are in this zone
  expander: string?             # which SAS expander serves this zone
  bandwidth_gbps: float?        # aggregate bandwidth for this zone

SasExpander:
  chipset: string?              # "Broadcom SAS3x36", "LSI SAS2x36"
  phy_count: uint               # total PHY lanes
  uplink_phys: uint             # PHYs allocated to uplink (to HBA)
  drive_phys: uint              # PHYs allocated to drives
  link_speed_gbps: float        # per-PHY speed (6, 12, 22.5)
  cascade: bool?                # is this expander cascaded behind another?
  firmware: FirmwareInfo?       # expander firmware
  zoning: bool?                 # SAS zoning support

EnclosureUplink:
  connector: string             # "mini_sas_hd_sff8643", "mini_sas_sff8088",
                                # "qsfp", "oculink", "pcie"
  lanes: uint                   # number of SAS/PCIe lanes in this uplink
  link_speed_gbps: float        # per-lane speed
  total_bandwidth_gbps: float   # lanes × speed = aggregate uplink bandwidth
  redundant_path: bool?         # dual-path to a second HBA/expander

EnclosureManagement:
  ses: bool?                    # SES (SCSI Enclosure Services) — temp, fan, power, bay status
  sgpio: bool?                  # SGPIO — per-bay LED control (locate, fault, activity)
  bmc: bool?                    # BMC/IPMI management interface
  protocol: string?             # "ses_2", "ses_3", "sgpio", "i2c"
```

**Worked example — 24-bay SAS JBOD connected via HBA**:

```
Host PCIe bus
  └── HBA: Broadcom SAS 9300-8i (PCIe x8 Gen 3 = 64 Gbps to host)
      ├── Port: mini-SAS HD 0 (4× SAS 12 Gbps = 48 Gbps)
      │   └── Cable: SFF-8643 to SFF-8643 → Enclosure Uplink A
      └── Port: mini-SAS HD 1 (4× SAS 12 Gbps = 48 Gbps)
          └── Cable: SFF-8643 to SFF-8643 → Enclosure Uplink B

Enclosure: SuperMicro 24-bay JBOD
  ├── Uplink A → SAS Expander 0 (24-port, SAS3x36)
  │   ├── 4 PHYs uplink (48 Gbps aggregate to HBA)
  │   └── 20 PHYs to backplane zone A (bays 0–11)
  │       └── 12 bays × 12 Gbps each, sharing 20× 12 Gbps = 240 Gbps backplane bandwidth
  │           (non-blocking for 12 drives — each gets full 12 Gbps)
  ├── Uplink B → SAS Expander 1 (backup path / zone B)
  │   ├── 4 PHYs uplink (48 Gbps aggregate to HBA)
  │   └── 20 PHYs to backplane zone B (bays 12–23)
  └── 24 drive bays (hot-swap, 3.5" SAS/SATA)
      ├── Bay 0:  Seagate Exos X18 16TB SAS (12 Gbps, dual-port)
      ├── Bay 1:  Seagate Exos X18 16TB SAS
      ├── ...
      └── Bay 23: Seagate Exos X18 16TB SAS
```

**What the routing graph captures from this**:

1. **Aggregate bandwidth**: 24 drives × 12 Gbps = 288 Gbps potential, but
   the uplinks carry 2× 48 Gbps = 96 Gbps. With 24 drives active, each
   drive gets ~4 Gbps effective uplink bandwidth (oversubscribed 3:1).
   For spinning drives (max ~250 MB/s = 2 Gbps), this is fine — the uplink
   isn't the bottleneck. For SSDs (up to 12 Gbps each), it would be.

2. **Zone awareness**: Bays 0–11 share expander 0's uplink. Bays 12–23
   share expander 1's uplink. Heavy I/O on bays 0–11 doesn't affect
   bays 12–23. The router knows which zone a drive is in.

3. **Multipath**: SAS dual-port drives can be reached via either expander.
   If expander 0 fails, drives in zone A are still accessible via a
   cascade through expander 1 (if configured). This maps to active
   redundancy (§2.13).

4. **HBA bandwidth bottleneck**: The HBA's PCIe x8 Gen 3 link gives
   64 Gbps to the host. Both uplinks combined carry 96 Gbps. If both
   zones are saturated, the PCIe link is the bottleneck. The graph
   traces this all the way to the CPU.

5. **SMART health per bay**: Each drive's SMART attributes feed into
   monitoring (§11). Trend analysis detects degrading drives before
   failure. The enclosure's SES data provides bay-level status (healthy,
   rebuilding, fault, locate LED).

6. **Firmware across the stack**: HBA firmware, SAS expander firmware,
   and individual drive firmware are all tracked (§14.1). A known
   expander firmware bug affecting I/O stability is flagged just like
   a known BIOS bug.

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

**AvrSpec** (AV receiver — the most complex compound consumer device):

An AV receiver is simultaneously an HDMI matrix switch (§2.5), an audio
processor (§2.13), a power amplifier (§2.10), a speaker router, and a
collection of media receivers (§2.9). It's modelled as a single compound
device with multiple sub-devices and a rich internal topology.

```yaml
AvrSpec:
  # --- HDMI switching ---
  hdmi: AvrHdmiSpec

  # --- Audio processing ---
  audio_processing: AvrAudioProcessingSpec

  # --- Amplification ---
  amplifier: AvrAmplifierSpec

  # --- Zones ---
  zones: AvrZone[]?             # multi-zone support (main + zone 2/3/4)

  # --- Streaming / media receivers ---
  streaming: AvrStreamingSpec?

  # --- Control ---
  control: AvrControlSpec

AvrHdmiSpec:
  inputs: AvrHdmiInput[]
  outputs: AvrHdmiOutput[]
  passthrough: bool?            # passes video through without processing
  upscaling: string?            # "none", "1080p", "4k", "8k"
  video_processing: string?     # vendor video processor ("Sigma Designs", "Analog Devices", "none")
  arc: bool?                    # ARC on output 1
  earc: bool?                   # eARC on output 1 (uncompressed Atmos)
  allm: bool?                   # Auto Low Latency Mode passthrough
  vrr: bool?                    # VRR passthrough
  qms: bool?                    # Quick Media Switching
  qft: bool?                    # Quick Frame Transport
  hdcp: string?                 # "hdcp_2.3", "hdcp_2.2"
  standby_passthrough: bool?    # HDMI passes through when AVR is in standby
  # The HDMI switch is modelled as §2.5 — the router can switch inputs
  # via IP/CEC/serial as part of pipeline activation.

AvrHdmiInput:
  id: string                    # "hdmi_1", "hdmi_2", etc.
  version: string?              # "hdmi_2.1", "hdmi_2.0"
  bandwidth_gbps: float?        # 48 (HDMI 2.1) or 18 (HDMI 2.0)
  max_resolution: Resolution?
  max_refresh: float?
  earc: bool?                   # some inputs support eARC (rare, usually outputs only)
  physical: PhysicalPortInfo?
  label: string?                # front panel label ("CBL/SAT", "GAME", "BD/DVD", "PC")
  # Vendor labels hint at intended use but don't restrict functionality.

AvrHdmiOutput:
  id: string                    # "hdmi_out_1", "hdmi_out_2"
  version: string?
  bandwidth_gbps: float?
  arc: bool?                    # ARC/eARC capable (typically output 1 only)
  earc: bool?
  zone: string?                 # "main", "zone_2" — which zone this output serves
  physical: PhysicalPortInfo?

AvrAudioProcessingSpec:
  # Decoding
  codecs: string[]?             # ["dolby_atmos", "dolby_truehd", "dolby_digital_plus",
                                #  "dts_x", "dts_hd_ma", "dts", "auro_3d",
                                #  "multichannel_pcm", "stereo_pcm"]
  max_channels: uint?           # maximum decoded channels (7.1, 9.1, 11.1)
  max_atmos_objects: uint?      # Dolby Atmos object count (if applicable)

  # Room correction
  room_correction: string?      # "audyssey_multeq_xt32", "audyssey_multeq_xt",
                                # "audyssey_multeq", "ypao_rsc",
                                # "ypao", "dirac_live", "anthem_arc",
                                # "mcacc", "dcac", "none"
  room_correction_mic: bool?    # measurement mic included?
  # Room correction data from the AVR can be read and compared against
  # Ozma's own room correction measurements (§2.13) for validation.

  # DSP
  bass_management: bool?        # crossover + sub routing
  configurable_crossover: bool? # per-speaker crossover frequency
  crossover_range_hz: { min: float, max: float }?
  parametric_eq: bool?          # manual parametric EQ
  parametric_eq_bands: uint?    # bands per channel
  dialog_enhancement: bool?
  night_mode: bool?             # dynamic range compression for quiet listening

  # Additional audio inputs (non-HDMI)
  analog_inputs: AvrAnalogInput[]?
  digital_inputs: AvrDigitalInput[]?
  phono_input: bool?            # MM phono preamp
  multi_channel_input: bool?    # 7.1 analog input (legacy)

AvrAnalogInput:
  id: string                    # "analog_1", "cd", "tuner"
  connector: string             # "rca_stereo", "xlr_stereo", "3.5mm"
  physical: PhysicalPortInfo?

AvrDigitalInput:
  id: string                    # "optical_1", "coax_1"
  connector: string             # "toslink", "coax_spdif", "aes_ebu"
  max_sample_rate: uint?        # TOSLINK caps at 96kHz for PCM, 48kHz for Dolby/DTS
  physical: PhysicalPortInfo?

AvrAmplifierSpec:
  channels: uint                # amplified channels (7, 9, 11, 13)
  power_per_channel_w: float?   # rated power (watts per channel, typically at 8Ω)
  impedance_ohm: float[]?       # supported impedances [4, 6, 8]
  class: string?                # "ab", "d", "g_h"
  total_power_w: float?         # maximum total power draw
  speaker_outputs: AvrSpeakerOutput[]
  pre_outs: AvrPreOut[]?        # pre-amp outputs for external amplification
  bi_amp: bool?                 # bi-amplification support (uses 2 channels per speaker)

AvrSpeakerOutput:
  id: string                    # "front_left", "front_right", "center", "surround_left", etc.
  channel: string               # ITU-R channel label ("FL", "FR", "FC", "SL", "SR", etc.)
  connector: string             # "binding_post", "spring_clip", "speakon"
  assignable: bool?             # can this output be reassigned to a different channel?
  # Assignable outputs are common — a 9.2 AVR with 9 amp channels can
  # be configured as 7.2.2 (Atmos height) or 7.2+Zone2 or 5.2.4, etc.

AvrPreOut:
  id: string                    # "sub_1", "sub_2", "zone_2_pre", "front_pre"
  channel: string               # what this pre-out carries
  connector: string             # "rca"
  physical: PhysicalPortInfo?

AvrZone:
  id: string                    # "main", "zone_2", "zone_3"
  name: string?
  independent_source: bool      # can this zone play a different source than main?
  sources: string[]?            # available sources for this zone
  output_type: string           # "amplified" (speaker binding posts),
                                # "pre_out" (RCA to external amp),
                                # "hdmi" (HDMI output 2)
  volume_control: bool
  max_channels: uint?           # zone 2 is typically stereo only

AvrStreamingSpec:
  airplay: bool?                # AirPlay 2
  spotify_connect: bool?
  chromecast: bool?             # Chromecast built-in
  bluetooth: BluetoothSpec?     # A2DP sink
  dlna: bool?                   # DLNA/UPnP renderer
  vendor_platform: string?      # "heos", "musiccast", "sonos_ready", "bluesound"
  internet_radio: bool?         # vTuner, TuneIn, etc.
  usb_playback: bool?           # USB-A for flash drive playback
  ethernet: bool?
  wifi: WifiSpec?

AvrControlSpec:
  ip: bool?                     # HTTP API / Telnet
  ip_protocol: string?          # "denon_avr", "onkyo_iscp", "yamaha_ynca",
                                # "marantz_ip", "anthem_ip", "nad_bluos"
  serial: bool?                 # RS-232 (DB-9 or 3.5mm)
  serial_protocol: string?      # vendor command set
  cec: bool?                    # HDMI CEC
  ir: bool?                     # IR remote
  ir_code_set: string?          # "denon", "marantz", "onkyo", "yamaha", "sony"
  vendor_app: string?           # "heos", "musiccast", "sonos", "bluos"
  home_automation: string[]?    # ["control4", "crestron", "savant", "amx", "ip_control"]
  # Most AVRs have IP control with a documented (or reverse-engineered)
  # protocol. This makes them confirmed controllable (§2.5) — the router
  # can switch inputs, change volume, select source, and read state.
```

**The AVR in the routing graph**:

An AVR creates multiple sub-devices in the graph:

1. **HDMI switch**: `type: switch` with HDMI inputs and outputs, `controllability:
   confirmed` via IP/CEC. The router switches HDMI inputs as part of
   scenario activation — "switch AVR to HDMI 3 (gaming PC)".

2. **Audio processor**: `type: audio_processor` — audio extracted from
   the active HDMI input, decoded (Atmos/DTS:X/PCM), processed (room
   correction, bass management, EQ), and routed to speakers.

3. **Amplifier**: Power delivery to speakers, modelled via §2.10 power
   model. 9×150W at 8Ω from a device drawing up to 800W.

4. **Speaker outputs**: Each speaker output is a sink port in the graph.
   The speaker arrangement (§2.13) uses the AVR's speaker configuration
   (which channels are active, distances, levels from Audyssey/YPAO)
   as the authoritative source — the AVR has already calibrated.

5. **Media receivers**: AirPlay 2, Spotify Connect, DLNA, Bluetooth —
   each is a `media_receiver` device (§2.9) discoverable via mDNS/UPnP.

6. **Zones**: Zone 2/3 are independent audio paths with their own source
   selection and volume. Each zone is a sub-device with its own ports.

**Example — Ozma controlling a home theatre**:

```
Scenario: "Movie Night"
  → AVR: switch to HDMI 1 (Apple TV)           [via IP command]
  → AVR: set audio mode to Dolby Atmos          [via IP command]
  → AVR: volume to -25 dB                       [via IP command]
  → TV: switch to HDMI 1 (AVR output)           [via CEC]
  → Lights: dim to 10%                          [via HA integration]

Scenario: "Gaming"
  → AVR: switch to HDMI 3 (Gaming PC)           [via IP command]
  → AVR: set audio mode to Game (low latency)   [via IP command]
  → TV: enable ALLM                             [via CEC passthrough]
  → AVR: Zone 2 source = Spotify Connect         [via IP command]
  → Zone 2 speakers play background music while gaming in main zone
```

**AudioInterfaceSpec** (pro audio interfaces — USB, Thunderbolt, FireWire,
PCIe. These are compound devices with multiple I/O types, internal routing
matrices, DSP, and sample-rate-dependent channel counts):

```yaml
AudioInterfaceSpec:
  connection: string            # "usb_c", "usb_b", "thunderbolt", "firewire_400",
                                # "firewire_800", "pcie", "dante_network", "aes67_network"
  driver: string?               # "class_compliant" (USB Audio Class 2/3, driverless),
                                # "vendor" (ASIO/CoreAudio driver required),
                                # "vendor_with_mixer" (driver + software mixer app)
  driver_name: string?          # "Focusrite Control", "RME TotalMix FX",
                                # "Universal Audio Console", "MOTU CueMix 5"

  # --- I/O at each sample rate (channel counts change!) ---
  io_configurations: IoConfiguration[]

  # --- Analog inputs ---
  analog_inputs: AnalogInput[]

  # --- Analog outputs ---
  analog_outputs: AnalogOutput[]

  # --- Digital I/O ---
  digital_io: DigitalIo[]?

  # --- Headphone outputs ---
  headphone_outputs: HeadphoneOutput[]?

  # --- MIDI ---
  midi_ports: MidiPort[]?

  # --- Clock ---
  clock: InterfaceClockSpec

  # --- Internal DSP ---
  dsp: InterfaceDspSpec?

  # --- Internal routing/mixer ---
  mixer: InterfaceMixerSpec?

  # --- Monitoring ---
  direct_monitoring: bool?      # zero-latency hardware monitoring (bypasses computer)
  talkback_mic: bool?           # built-in talkback microphone

IoConfiguration:
  sample_rate_range: { min: uint, max: uint }  # e.g., 44100–48000
  analog_in: uint               # analog input channels at this rate
  analog_out: uint              # analog output channels
  adat_in: uint?                # ADAT input channels (8 @ 48k, 4 @ 96k SMUX, 2 @ 192k)
  adat_out: uint?
  spdif_in: uint?               # S/PDIF input channels (always 2)
  spdif_out: uint?
  aes_in: uint?                 # AES3 input channels
  aes_out: uint?
  madi_in: uint?                # MADI channels (64 @ 48k, 32 @ 96k)
  madi_out: uint?
  dante_in: uint?               # Dante/AES67 network channels
  dante_out: uint?
  total_in: uint                # total input channels (sum of all)
  total_out: uint               # total output channels
  usb_bandwidth_required: string? # "usb2_sufficient", "usb3_required"
  # CRITICAL: channel counts change with sample rate. An interface with
  # 18 inputs at 48kHz may have only 10 at 96kHz (ADAT drops from 8→4
  # channels in SMUX mode) and 4 at 192kHz. The router must check the
  # active sample rate to know the actual channel count.

AnalogInput:
  id: string                    # "input_1", "mic_1", "inst_1", "line_5"
  channel_number: uint          # DAW channel number (1-indexed)
  type: string[]                # ["mic", "line", "instrument", "hi_z"]
                                # Many inputs are switchable between types
  connector: string             # "xlr", "xlr_trs_combo", "trs_6.35mm", "rca",
                                # "trs_3.5mm", "din"
  phantom_power: bool?          # 48V phantom (per-input switchable on good interfaces)
  phantom_group: string?        # if phantom is grouped ("1-4", "5-8", "all")
  pad_db: float?                # input pad (-10dB, -20dB)
  gain_range_db: { min: float, max: float }?  # preamp gain range
  impedance: InputImpedance?    # input impedance (affects sound, especially for mics/guitars)
  max_input_level_dbu: float?   # maximum input level before clipping
  dynamic_range_db: float?      # measured dynamic range (A-weighted)
  thd_percent: float?           # total harmonic distortion + noise
  frequency_response: FrequencyResponse?
  physical: PhysicalPortInfo?

InputImpedance:
  mic_ohm: float?               # microphone impedance (typically 1.5–3 kΩ)
  line_ohm: float?              # line level impedance (typically 10–20 kΩ)
  instrument_ohm: float?        # instrument/hi-Z (typically 500 kΩ – 1 MΩ)
  # Impedance matching matters for guitar DI and ribbon mics.
  # A low-impedance guitar input sounds thin on a 10kΩ line input.
  # A 1 MΩ hi-Z input is correct. The database captures this per input.

AnalogOutput:
  id: string                    # "output_1", "main_l", "monitor_a_l"
  channel_number: uint          # DAW channel number
  type: string                  # "line", "main", "monitor", "aux"
  connector: string             # "xlr", "trs_6.35mm", "rca", "trs_3.5mm"
  balanced: bool?               # balanced (XLR, TRS) or unbalanced (RCA, TS)
  max_output_level_dbu: float?  # maximum output level
  impedance_ohm: float?         # output impedance
  physical: PhysicalPortInfo?
  assignable: bool?             # can be reassigned to different mix/bus in software

HeadphoneOutput:
  id: string                    # "hp_1", "hp_2"
  connector: string             # "trs_6.35mm", "trs_3.5mm"
  impedance_range_ohm: { min: float, max: float }?  # can drive headphones from X to Y ohm
  max_power_mw: float?          # at reference impedance
  independent_source: bool      # can play a different mix than main outputs?
  independent_volume: bool      # has its own volume control?
  physical: PhysicalPortInfo?

DigitalIo:
  id: string                    # "adat_1", "spdif_1", "aes_1", "madi_1", "dante_1"
  type: string                  # "adat", "spdif_coax", "spdif_optical", "aes3",
                                # "madi_bnc", "madi_optical", "madi_sc",
                                # "dante", "aes67", "ravenna"
  direction: string             # "input", "output", "bidirectional"
  connector: string             # "toslink", "bnc", "rca", "xlr", "sc_fiber", "rj45"
  channels_at_48k: uint?        # channels at standard rate
  channels_at_96k: uint?        # channels at double rate (SMUX)
  channels_at_192k: uint?       # channels at quad rate (SMUX4)
  physical: PhysicalPortInfo?
  # ADAT: 8ch @ 44.1/48k, 4ch @ 88.2/96k (SMUX), 2ch @ 176.4/192k (SMUX4)
  # MADI: 64ch @ 48k, 32ch @ 96k
  # S/PDIF: always 2ch, up to 192kHz
  # AES3: 2ch per cable, up to 192kHz

MidiPort:
  id: string                    # "midi_1", "midi_in", "midi_out"
  direction: string             # "input", "output", "bidirectional"
  connector: string             # "din_5pin", "trs_3.5mm_type_a", "trs_3.5mm_type_b", "usb"
  physical: PhysicalPortInfo?

InterfaceClockSpec:
  internal: bool                # can be clock master (internal crystal)
  word_clock_in: bool?          # BNC word clock input
  word_clock_out: bool?         # BNC word clock output
  word_clock_thru: bool?        # BNC word clock pass-through
  sync_sources: string[]?       # ["internal", "word_clock", "adat", "spdif", "aes",
                                # "madi", "dante", "ptp"]
  superclock: bool?             # 256× superclock output (Digidesign/Avid legacy)
  # The interface's clock source determines the sample rate for the entire
  # audio chain. When an interface is word clock slave to an external master,
  # its sample rate is set by the master. This feeds into §7 Clock Model.
  # On PipeWire, the interface's clock maps to a driver node — it determines
  # PipeWire's graph scheduling rate.

InterfaceDspSpec:
  type: string?                 # "sharc", "fpga", "arm", "custom_asic"
  processor: string?            # "Analog Devices SHARC", "Xilinx FPGA", "UA custom"
  processing_power: string?     # qualitative ("16 plugins per channel", "full mix")
  plugins: string[]?            # built-in DSP plugins ("eq", "compressor", "reverb",
                                # "amp_sim", "channel_strip", "de-esser")
  vendor_plugin_format: string? # "uad", "antelope_fpga", "metric_halo"
  latency_samples: uint?        # DSP processing latency in samples
  realtime: bool?               # DSP processing is real-time (no added buffer)
  # Some interfaces (UA Apollo, Antelope, Metric Halo) run DSP plugins
  # on dedicated hardware inside the interface. These don't consume
  # host CPU — they're modelled as audio processors (§2.13) running
  # on the interface's own compute resources (§2.7).

InterfaceMixerSpec:
  matrix: bool                  # full N×M routing matrix?
  inputs_to_mix: uint?          # how many sources can feed the mixer
  mix_buses: uint?              # independent mix buses (for headphone mixes, etc.)
  per_channel: string[]?        # ["gain", "pan", "mute", "solo", "eq", "compressor",
                                # "reverb_send", "phase", "phantom"]
  total_mix_recall: bool?       # mixer state saved in hardware and recalled on boot
  software_controlled: bool     # mixer is configured via host software
  standalone_operation: bool?   # mixer works without host computer connected
  # RME TotalMix FX: full matrix, 3 hardware submixes, per-channel EQ/comp,
  #   standalone operation, recall on boot. This is a monitor controller.
  # Focusrite Control: simpler routing, per-output source selection,
  #   loopback routing. Software-only.
  # UA Console: DSP-accelerated channel strip + monitor section,
  #   insert effects on input, talkback mic routing.
```

**How the routing graph models an audio interface**:

An audio interface creates many ports in the graph — every analog input,
every analog output, every digital I/O channel, every headphone output,
and every MIDI port is an independent port with its own format capabilities.

```
Audio Interface: Focusrite Scarlett 18i20 (USB-C)
├── Analog inputs (8 ports):
│   ├── input_1: XLR/TRS combo, mic/line/inst, 48V, gain 0–56dB → audio source port
│   ├── input_2: XLR/TRS combo, mic/line/inst, 48V, gain 0–56dB → audio source port
│   ├── input_3–4: XLR/TRS combo, mic/line, 48V (grouped 3-4) → audio source ports
│   └── input_5–8: TRS line only → audio source ports
├── Analog outputs (10 ports):
│   ├── output_1–2: TRS balanced, line → audio sink ports (main monitors)
│   ├── output_3–10: TRS balanced, line → audio sink ports (assignable)
│   └── hp_1, hp_2: 6.35mm TRS, independent source/volume → audio sink ports
├── Digital I/O:
│   ├── ADAT in: 8ch @ 48kHz / 4ch @ 96kHz → audio source ports
│   ├── ADAT out: 8ch @ 48kHz / 4ch @ 96kHz → audio sink ports
│   ├── S/PDIF in: 2ch → audio source ports
│   └── S/PDIF out: 2ch → audio sink ports
├── MIDI: DIN in + DIN out → control ports
├── Clock: internal + ADAT + S/PDIF sync → clock domain master/slave
└── Internal mixer: 20 in × 12 out matrix → virtual mix bus devices
```

At 48kHz, this is 20 inputs and 12 outputs. At 96kHz, it's 14 inputs and
12 outputs (ADAT halves). The router's format negotiation (§4.3) handles
this — when the interface's sample rate changes, its FormatSet changes,
the port count changes, and affected pipelines are re-evaluated.

**Internal mixer as graph devices**: The interface's internal mixer (Focusrite
Control, RME TotalMix FX, UA Console) maps to mix buses (§2.13) running on
the interface's hardware — not on the host CPU. These are `audio_processor`
devices with `resource_cost` on the interface, not on the controller.
RME TotalMix FX is effectively a full monitor controller running inside
the interface — source selection, per-channel EQ/comp, 3 independent
submixes for headphone monitoring, and it works standalone without a
computer. The graph models this as a monitor controller (§2.13) whose
control path (§2.12) goes through the vendor's software.

**Word clock in the graph**: An interface's clock source feeds into the
clock model (§7). When the interface is word clock master, it sets the
sample rate for the entire audio chain. When it's slave (to a studio
word clock generator or another interface), it follows. The routing graph
tracks the clock chain: word clock generator → interface A → interface B
(via ADAT) → PipeWire (locked to interface A's clock as the driver node).

**FireWire note**: FireWire (IEEE 1394) audio interfaces are still in use
in studios (RME Fireface, MOTU 828, Metric Halo LIO-8). FireWire is a
transport with its own isochronous scheduling — guaranteed bandwidth
without contention, unlike USB. The `firewire_400` and `firewire_800`
connections map to transport plugins with characteristics:
- FireWire 400: 400 Mbps, <1ms latency, isochronous
- FireWire 800: 800 Mbps, <1ms latency, isochronous
- No bandwidth sharing with non-audio traffic (unlike USB)
- Daisy-chainable (unlike USB)

**TransceiverSpec** (SFP/SFP+/SFP28/QSFP+/QSFP28/QSFP-DD pluggable
optics and DAC cables):

Transceiver modules are pluggable devices that sit in a cage on a switch,
NIC, or HBA. They determine the actual link speed, reach, and media type.
A 10GbE NIC with an SFP+ cage can run at 1G or 10G depending on the
inserted module, over fiber or copper depending on the module type.

```yaml
TransceiverSpec:
  form_factor: string           # "sfp", "sfp_plus", "sfp28", "qsfp_plus",
                                # "qsfp28", "qsfp_dd", "osfp", "cfp", "xfp"
  type: string                  # "optical", "dac" (Direct Attach Copper),
                                # "aoc" (Active Optical Cable), "copper_rj45"
  speed_gbps: float             # line rate (1, 10, 25, 40, 100, 200, 400)
  protocol: string[]?           # ["ethernet", "fibre_channel", "infiniband",
                                # "sonet", "otu"]
  wavelength_nm: uint?          # optical wavelength (850, 1310, 1550, CWDM/DWDM)
  reach: TransceiverReach       # maximum distance
  fiber_type: string?           # "multimode_om3", "multimode_om4", "singlemode_os2"
  connector: string?            # "lc_duplex", "mpo_mtp", "none" (DAC has integral cable)
  power_draw_w: float?          # module power consumption (1–3.5W typical, high for QSFP-DD)
  coding: string?               # "nrz", "pam4" (25G+ uses PAM4)
  duplex: bool?                 # true for most; false for BiDi (single fiber, two wavelengths)
  breakout: BreakoutSpec?       # for QSFP: can split into multiple lower-speed links

  # --- EEPROM identity (MSA A0h — read from module I2C address 0x50) ---
  eeprom: TransceiverEeprom?

  # --- Diagnostics (MSA A2h — read from module I2C address 0x51) ---
  dom: TransceiverDom?

  # --- Firmware and coding ---
  firmware: TransceiverFirmware?

  # --- Vendor lock and compatibility ---
  vendor_lock: VendorLockSpec?

TransceiverEeprom:
  # SFP MSA defines a 256-byte EEPROM at I2C address 0x50 (A0h).
  # This is the module's identity — readable on any standards-compliant host.
  vendor_name: string?          # bytes 20–35 (ASCII, space-padded)
  vendor_oui: string?           # bytes 37–39 (IEEE OUI)
  vendor_pn: string?            # bytes 40–55 (vendor part number)
  vendor_rev: string?           # bytes 56–59 (revision)
  vendor_sn: string?            # bytes 68–83 (serial number)
  date_code: string?            # bytes 84–91 (YYMMDD + lot code)
  transceiver_type: uint?       # byte 6 (SFF-8472 type code)
  connector_type: uint?         # byte 2 (LC=7, SC=1, copper=0x21, etc.)
  encoding: uint?               # byte 11 (8b10b, 64b66b, NRZ, PAM4)
  bit_rate_nominal_mbps: uint?  # byte 12 × 100 (or byte 66 for >25.4 Gbps)
  length_smf_km: uint?          # byte 14 (single-mode reach in km)
  length_smf_100m: uint?        # byte 15 (single-mode reach in 100m units)
  length_om3_10m: uint?         # byte 19 (OM3 reach in 10m units)
  length_om2_10m: uint?         # byte 16 (OM2 reach)
  length_om1_10m: uint?         # byte 17 (OM1 reach)
  length_copper_m: uint?        # byte 18 (copper reach in metres)
  # The agent reads this via ethtool (Linux), PowerShell (Windows), or
  # switch CLI / SNMP. Every inserted SFP module auto-identifies.
  raw_a0h: bytes?               # full 256-byte A0h dump (for advanced analysis)

TransceiverDom:
  # Digital Optical Monitoring — real-time module health.
  # Read from I2C address 0x51 (A2h), or pages on QSFP+.
  supported: bool               # does this module support DOM?
  calibration: string?          # "internal" (module calibrates), "external" (host calibrates)
  # Current readings (updated periodically by the agent):
  temperature_c: float?         # module temperature
  voltage_v: float?             # supply voltage (typically 3.3V)
  tx_bias_ma: float?            # laser bias current
  tx_power_dbm: float?          # transmit optical power
  rx_power_dbm: float?          # receive optical power
  # Alarm and warning thresholds (from module EEPROM):
  thresholds: DomThresholds?

DomThresholds:
  # Each metric has high/low alarm and high/low warning thresholds
  # stored in the module EEPROM. When exceeded, the module sets flag bits.
  temp_high_alarm_c: float?
  temp_low_alarm_c: float?
  temp_high_warn_c: float?
  temp_low_warn_c: float?
  voltage_high_alarm_v: float?
  voltage_low_alarm_v: float?
  tx_bias_high_alarm_ma: float?
  tx_bias_low_alarm_ma: float?
  tx_power_high_alarm_dbm: float?
  tx_power_low_alarm_dbm: float?
  rx_power_high_alarm_dbm: float?
  rx_power_low_alarm_dbm: float?
  rx_power_low_warn_dbm: float?
  # These thresholds feed into the monitoring system (§11). An RX power
  # reading below the low warning threshold triggers a trend alert:
  # "Fiber link to switch-2 RX power -18.3 dBm (warning threshold -17 dBm).
  # Check fiber connectors and patch cables."

TransceiverFirmware:
  firmware_version: string?     # module firmware version (coded/smart modules only)
  firmware_updateable: bool     # can the EEPROM/firmware be reflashed?
  flash_tool: string?           # tool used to flash ("sfp_reflash", "mikrotik_cli",
                                # "fs_sfp_tool", "flexoptix_tool", "vendor_proprietary")
  # --- Reflashing / recoding ---
  # Many third-party SFP modules can be reflashed to change the vendor
  # coding stored in the EEPROM. This is used to:
  # 1. Bypass vendor lock (Cisco/Juniper/Arista reject non-OEM modules)
  # 2. Change the reported speed/type for compatibility
  # 3. Update module firmware for bug fixes
  # The device database tracks known reflash compatibility:
  recoding: RecodingSpec?

RecodingSpec:
  eeprom_writable: bool         # is the A0h EEPROM writable?
  password_protected: bool?     # EEPROM write requires password (some modules)
  default_password: string?     # factory default password (often "00000000" or "FFFFFFFF")
  known_tools: string[]?        # ["mikrotik_routeros", "linux_i2c", "flexoptix",
                                # "fs_com_tool", "sfp_diag", "taobao_programmer"]
  vendor_code_targets: VendorCode[]?  # known vendor codings this module can be set to
  risks: string?                # "safe — EEPROM only, laser unaffected",
                                # "moderate — wrong coding may disable TX",
                                # "dangerous — can permanently brick module"
  community_notes: string?      # "FS.com modules accept Cisco/Juniper/HPE coding
                                #  via their free online tool. No hardware programmer needed."

VendorCode:
  target_vendor: string         # "cisco", "juniper", "arista", "hpe", "dell",
                                # "ubiquiti", "mikrotik", "mellanox"
  coding_bytes: string?         # hex bytes to write (if known and safe to publish)
  tool: string?                 # tool to use for this specific coding
  verified: bool?               # community verified to work
  switch_models: string[]?      # specific switch models confirmed working
  notes: string?

VendorLockSpec:
  locked: bool                  # does the host device reject third-party modules?
  behavior: string?             # "rejected" (won't link), "warning_only" (works but logs error),
                                # "speed_limited" (forced to lower speed), "no_dom" (works but
                                # diagnostics disabled), "accepted" (no vendor check)
  bypass_method: string?        # "recode_eeprom", "cli_command", "unsupported_transceiver_allow",
                                # "none_known"
  # Vendor lock behavior:
  # Cisco: rejects by default, "service unsupported-transceiver" enables
  # Juniper: warns but works (most models)
  # Arista: warns but works
  # HPE/Aruba: rejects by default on some models
  # Ubiquiti: accepts most third-party
  # MikroTik: accepts everything, can even reflash EEPROMs from the CLI

TransceiverReach:
  max_distance_m: float         # maximum link distance
  typical_distance_m: float?    # typical deployment distance
  distance_class: string?       # "sr" (short reach, <300m), "lr" (long reach, <10km),
                                # "er" (extended, <40km), "zr" (zero-dispersion, <80km)

BreakoutSpec:
  mode: string                  # "4x10g", "4x25g", "2x50g", "8x50g", "4x100g"
  cables: string                # "mpo_to_4xlc" (breakout cable), "direct"
  # A QSFP+ (40G) can break out into 4× SFP+ (10G) with a breakout cable.
  # A QSFP28 (100G) can break out into 4× SFP28 (25G).
  # The routing graph models each breakout lane as a separate link.
```

**Common transceiver types**:

| Module | Speed | Type | Reach | Fiber | Use case |
|--------|-------|------|-------|-------|----------|
| SFP SX | 1G | Optical | 550m | OM3/OM4 MM | Short-reach 1G |
| SFP LX | 1G | Optical | 10km | OS2 SM | Long-reach 1G |
| SFP-T | 1G | Copper RJ45 | 100m | — | 1G over existing Cat5e+ |
| SFP+ SR | 10G | Optical | 300m (OM3), 400m (OM4) | MM | Data centre 10G |
| SFP+ LR | 10G | Optical | 10km | OS2 SM | Building-to-building 10G |
| SFP+ DAC | 10G | Direct copper | 1–7m | — | Rack-to-rack (cheapest 10G) |
| SFP28 SR | 25G | Optical | 100m | OM3/OM4 | 25G server links |
| QSFP+ SR4 | 40G | Optical | 150m | OM3/OM4 | 40G uplinks |
| QSFP28 SR4 | 100G | Optical | 100m | OM3/OM4 | 100G spine |
| QSFP28 LR4 | 100G | Optical (CWDM) | 10km | OS2 SM | 100G WAN |
| QSFP28 DAC | 100G | Direct copper | 1–5m | — | Top-of-rack (cheapest 100G) |

**DOM (Digital Optical Monitoring)**: Most SFP+ and above transceivers
report real-time diagnostics — TX/RX optical power (dBm), temperature,
laser bias current, supply voltage. These feed into the monitoring system
(§11) as `measured` quality link metrics. Declining RX power over time is
a trend alert: "Fiber link to switch-2 showing 2 dB loss increase over
6 months — check connectors or patch cable."

**FiberCableSpec** (fiber optic cables — the physical fiber between
transceivers):

```yaml
FiberCableSpec:
  fiber_type: string            # "multimode", "singlemode"
  grade: string                 # "om1", "om2", "om3", "om4", "om5",
                                # "os1", "os2"
  core_um: float                # core diameter (50µm for OM3/OM4, 62.5µm for OM1/OM2,
                                #                9µm for OS1/OS2)
  cladding_um: float?           # typically 125µm
  strand_count: uint?           # fiber count (2 for duplex, 12/24 for MPO/MTP trunks)
  connector_a: string           # connector on end A ("lc", "sc", "mpo_12", "mpo_24", "st")
  connector_b: string           # connector on end B (may differ — LC to SC patch)
  polish: string?               # "upc" (flat, blue), "apc" (angled, green — singlemode only)
  length_m: float
  jacket: string?               # "ofnr" (riser), "ofnp" (plenum), "lszh", "outdoor_armored",
                                # "outdoor_direct_burial"
  bend_radius_mm: float?        # minimum bend radius
  attenuation_db_per_km: float? # fiber attenuation (OM3: 3.5 @ 850nm, OS2: 0.4 @ 1310nm)
  bandwidth_mhz_km: float?      # modal bandwidth (OM3: 2000, OM4: 4700 @ 850nm)
  # Higher bandwidth_mhz_km = longer reach at higher speeds.
  # OM3 supports 10G at 300m. OM4 supports 10G at 400m. Same speed, more margin.
```

**Fiber grade capabilities**:

| Grade | Core | 1G reach | 10G reach | 25G reach | 40G reach | 100G reach | Notes |
|-------|------|----------|-----------|-----------|-----------|------------|-------|
| OM1 | 62.5µm | 275m | 33m | — | — | — | Legacy. Orange jacket. Avoid for new install. |
| OM2 | 50µm | 550m | 82m | — | — | — | Legacy. Orange jacket. |
| OM3 | 50µm | 1000m | 300m | 100m | 150m | 100m | Aqua jacket. Current standard. |
| OM4 | 50µm | 1000m | 400m | 100m | 150m | 150m | Aqua/violet. Better bandwidth. |
| OM5 | 50µm | 1000m | 400m | 100m | 150m | 400m | Lime green. SWDM wideband. |
| OS1 | 9µm | 10km | 10km | 10km | 10km | 10km | Yellow. Indoor singlemode. |
| OS2 | 9µm | 10km+ | 10km+ | 10km+ | 10km+ | 40km+ | Yellow. Indoor/outdoor singlemode. |

The router uses this to validate: "Your OM1 fiber between buildings can't
sustain 10G at 200m. You need OM3 or better. Or use singlemode (OS2) with
LR transceivers."

**ServerChassisSpec** (1U–4U rackmount servers, blade chassis, multi-node):

```yaml
ServerChassisSpec:
  form_factor: string           # "1u", "2u", "4u", "blade_chassis", "tower_server",
                                # "multi_node_2u4n", "multi_node_1u2n"
  rack_units: uint              # height in U (1, 2, 4, 7, 10 for blade chassis)
  depth_mm: uint?               # chassis depth (short ~500mm, standard ~700mm, deep ~900mm)
  width: string?                # "19_inch" (standard), "open_compute", "proprietary"

  # --- Compute ---
  motherboard_form_factor: string?  # "proprietary_server", "atx", "eatx",
                                    # "blade_module", "sled"
  cpu_sockets: uint?            # 1, 2, 4
  max_memory_slots: uint?
  max_memory_tb: float?

  # --- Storage ---
  drive_bays: DriveBayGroup[]?  # front + rear drive bays
  backplane: BackplaneSpec?     # SAS/SATA/NVMe backplane (from §15 StorageSpec)
  hot_swap_drives: bool?
  rear_drive_bays: uint?        # some 1U chassis have 2× rear 2.5" bays

  # --- Expansion ---
  pcie_slots: ExpansionSlot[]?  # PCIe risers and slots
  riser_cards: RiserConfig[]?   # which risers are installed (determines available slots)
  ocp_slots: uint?              # OCP 3.0 NIC slots

  # --- Power ---
  psu_bays: PsuBay[]
  redundant_psu: bool?          # N+1 PSU redundancy
  hot_swap_psu: bool?           # PSUs are hot-swappable

  # --- Cooling ---
  fan_zones: ThermalZone[]?     # reuses ThermalTopology model
  hot_swap_fans: bool?
  redundant_fans: bool?

  # --- Management ---
  bmc: BmcSpec?                 # BMC/IPMI/iLO/iDRAC/AMT

  # --- Front panel ---
  front_io: PhysicalPort[]?     # front panel ports (VGA, USB, serial, ID button)
  rear_io: PhysicalPort[]?      # rear panel ports
  status_leds: string[]?        # ["power", "health", "nic", "uid", "fault"]

DriveBayGroup:
  position: string              # "front", "rear", "internal"
  form_factor: string           # "3.5_inch", "2.5_inch", "edsff_e1s", "edsff_e3s"
  count: uint
  hot_swap: bool
  interface: string[]           # ["sas", "sata", "nvme", "u2"]
  backplane_zones: string[]?    # which backplane zones these bays connect to

PsuBay:
  id: string
  form_factor: string           # "crps" (Common Redundant Power Supply),
                                # "atx", "proprietary_server", "flex_atx"
  max_wattage_w: uint?
  hot_swap: bool
  populated: string?            # device database ID of installed PSU

RiserConfig:
  id: string                    # "riser_1", "riser_2"
  type: string                  # vendor-specific riser model
  slots: ExpansionSlot[]        # what PCIe slots this riser provides
  # Server PCIe risers are specific to the chassis model. A Dell R740
  # has 3 riser options that provide different slot configurations.
  # The device database entry for the chassis lists available risers
  # and what slots each provides.

BmcSpec:
  type: string                  # "ipmi_2.0", "ilo_5", "ilo_6", "idrac_9",
                                # "idrac_8", "amt", "openbmc", "aspeed"
  network: bool                 # dedicated management NIC
  shared_nic: bool?             # shares a NIC with host OS
  vlan: bool?                   # management on separate VLAN
  virtual_media: bool?          # remote ISO mount
  virtual_console: bool?        # remote KVM (HTML5/Java)
  sol: bool?                    # Serial over LAN
  firmware: FirmwareInfo?       # BMC firmware version
  # BMC is both a control path (§2.12) — commands reach the server via
  # BMC even when the OS is down — and a monitoring source (temperature,
  # fan speed, PSU status, event log). It maps to a sub-device in the graph
  # with its own network port, firmware, and known vulnerabilities.
```

**RackSpec** (expanded from FurnitureSpec — racks deserve their own model):

```yaml
RackSpec:
  rack_type: string             # "enclosed_4post", "enclosed_2post",
                                # "open_4post", "open_2post", "wall_mount",
                                # "desktop", "portable", "lackrack"
  units: uint                   # total rack units (4, 6, 8, 12, 15, 18, 22, 25, 42, 45, 48)
  width: string                 # "19_inch" (standard), "10_inch" (SOHO), "23_inch" (telco)
  depth_mm: uint?               # external depth
  usable_depth_mm: uint?        # internal rail-to-rail depth (matters for server clearance)
  height_mm: uint?              # external height
  weight_capacity_kg: float?    # static weight capacity
  rolling_weight_kg: float?     # weight capacity on casters
  material: string?             # "steel", "aluminum", "wood", "ikea_lack"

  # --- Rails and mounting ---
  rail_type: string?            # "square_hole", "round_hole", "threaded",
                                # "cage_nut", "clip_nut"
  adjustable_depth: bool?       # rails can adjust front-to-rear spacing
  rail_depth_range_mm: { min: uint, max: uint }?

  # --- Cable management ---
  cable_management: CableManagementSpec?

  # --- Environment ---
  enclosed: bool                # has side panels and door(s)
  front_door: string?           # "mesh", "glass", "solid", "none"
  rear_door: string?            # "mesh", "split_mesh", "solid", "none"
  side_panels: bool?
  ventilation: string?          # "passive", "top_fan", "bottom_intake", "climate_controlled"
  lock: bool?                   # lockable doors

  # --- Power ---
  pdu_mounts: PduMount[]?       # where PDUs can be mounted
  power_inlet: PowerConnectorSpec?  # if the rack has a built-in power inlet

  # --- Layout ---
  unit_positions: RackUnit[]?   # what's in each U position

CableManagementSpec:
  vertical_managers: uint?      # vertical cable managers (0, 1, or 2 — left/right)
  horizontal_managers: uint?    # horizontal cable management panels between equipment
  cable_tray: bool?             # overhead or under-floor cable tray
  cable_rings: bool?            # cable rings on rear posts
  velcro_included: bool?
  brush_panels: uint?           # blanking panels with brush cable pass-through
  lacing_bars: uint?            # horizontal lacing bars

PduMount:
  position: string              # "rear_left", "rear_right", "rear_center",
                                # "side_left", "side_right"
  orientation: string           # "vertical_0u", "horizontal"
  max_length_mm: uint?          # maximum PDU length in this position

RackUnit:
  u_position: uint              # 1 = bottom, N = top (or configurable)
  occupied_by: string?          # device ID of what's installed here
  occupied_units: uint?         # how many U this device takes (1, 2, 4)
  blanking_panel: bool?         # blanking panel for airflow management
  notes: string?                # "reserved for future switch", "cable management"
```

**The LackRack**: An IKEA LACK side table ($10) whose legs happen to be
exactly 19" (483mm) apart — the same width as a standard server rack.
It's a legitimate entry in the device database:

```yaml
id: "ikea-lack-side-table"
type: rack
name: "IKEA LACK Side Table (LackRack)"
vendor: "IKEA"
model: "LACK"
rack:
  rack_type: "lackrack"
  units: 8                      # approximately 8U between shelf and legs
  width: "19_inch"              # 500mm external, 483mm between legs = 19" !!
  depth_mm: 550
  usable_depth_mm: 500
  height_mm: 550
  weight_capacity_kg: 25        # IKEA-rated. Your mileage may vary.
  material: "ikea_lack"         # particleboard + honeycomb paper fill
  rail_type: null               # no rails — equipment sits on the shelf or is screwed to legs
  adjustable_depth: false
  enclosed: false
  cable_management:
    vertical_managers: 0        # zip ties recommended
    cable_rings: false
  pdu_mounts: []                # velcro a power strip to the leg
tags: ["budget", "homelab", "meme", "surprisingly_functional"]
sources:
  - { type: "community", note: "The LackRack — eth-0.de/lackrack/" }
dimensions_mm: { w: 550, d: 550, h: 550 }
price_approximate: "$10"
notes: "Legs are exactly 19 inches apart. Not rated for heavy servers.
        Stack two for a full-height rack. Drill the shelf for cable routing.
        Has housed more production infrastructure than anyone wants to admit."
```

**RackAccessorySpec** (passive rack infrastructure — patch panels, shelves,
blanking panels, cable management, drawers, and other non-powered items
that occupy rack units or attach to rack structure):

```yaml
RackAccessorySpec:
  accessory_type: string        # see table below
  rack_units: uint?             # height in U (0 for non-U-mounted items like vertical managers)
  mounting: string?             # "front", "rear", "front_and_rear", "side", "vertical", "toolless"
  depth_mm: uint?

  # --- Patch panel specific ---
  patch_panel: PatchPanelSpec?

  # --- Shelf/drawer specific ---
  shelf: RackShelfSpec?

  # --- Blanking/airflow specific ---
  blanking: BlankingSpec?

  # --- Cable management specific ---
  cable_mgmt: CableMgmtPanelSpec?

# Accessory types:
# | Type | U | Description |
# |------|---|-------------|
# | patch_panel_copper | 1–2 | Copper patch panel (Cat5e/6/6a/8) with keystone or punch-down |
# | patch_panel_fiber | 1–2 | Fiber patch panel / splice enclosure (LC/SC/MPO) |
# | patch_panel_coax | 1 | Coaxial patch panel (BNC/F-type) |
# | patch_panel_xlr | 1–2 | XLR audio patch panel |
# | patch_panel_trs | 1–2 | TRS/bantam audio patchbay |
# | patch_panel_blank | 1 | Blank keystone panel (user populates with modules) |
# | shelf_fixed | 1–2 | Fixed shelf (for non-rack-mount equipment) |
# | shelf_sliding | 1–2 | Sliding shelf (pull out for access) |
# | shelf_vented | 1–2 | Vented shelf (airflow through) |
# | shelf_keyboard | 1 | Keyboard tray (sliding, with mouse area) |
# | drawer | 2–4 | Rack-mount drawer (tools, parts, documentation) |
# | blanking_panel | 1–3 | Solid blanking panel (airflow management) |
# | blanking_brush | 1 | Brush strip panel (cable pass-through with airflow seal) |
# | blanking_vented | 1 | Vented blanking panel (passive airflow) |
# | cable_manager_h | 1–2 | Horizontal cable management panel (D-rings, fingers) |
# | cable_manager_v | 0 | Vertical cable manager (side-mount, full rack height) |
# | lacing_bar | 0 | Horizontal lacing bar (bolt-on, cable tie-down) |
# | power_strip_mount | 0 | Bracket for mounting a power strip to rack rail |
# | rack_mount_ears | 0 | Ears/brackets to rack-mount non-rack equipment |
# | rail_kit | 0 | Sliding rail kit for specific server/device |
# | kvm_console | 1 | Rack-mount LCD + keyboard (fold-out) |

PatchPanelSpec:
  port_count: uint              # total ports (12, 24, 48)
  port_type: string             # "rj45_cat6a", "rj45_cat6", "rj45_cat5e",
                                # "rj45_cat8", "keystone_blank",
                                # "lc_duplex", "sc_duplex", "mpo",
                                # "bnc", "f_type",
                                # "xlr_3pin", "xlr_5pin", "trs_6.35mm",
                                # "trs_bantam", "rca"
  wiring_standard: string?      # "t568a", "t568b" (copper), "straight", "crossover"
  shielded: bool?               # shielded/STP (for Cat6a/Cat8)
  keystone: bool?               # keystone jack based (modular, replaceable)
  punch_down: string?           # "110", "krone", "lsa_plus" (punch-down block type)
  feed_through: bool?           # coupler/feed-through (no punch-down — RJ45 on both sides)
  loaded: bool?                 # shipped with jacks installed, or blank?
  numbering: string?            # port numbering scheme ("1-24_left_to_right",
                                # "1-24_top_bottom_zigzag")
  # Patch panels are passthrough devices in the graph — they add a hop
  # with near-zero latency but they define the physical cable topology.
  # Port 1 on the patch panel maps to a specific wall run or device.
  # The graph traces: switch port 1 → patch cable → patch panel port 1 →
  # structured cabling → wall plate → device. Every hop is visible.
  ports: PatchPort[]?           # per-port mapping (what each port connects to)

PatchPort:
  port_number: uint
  label: string?                # printed or handwritten label ("Desk 3", "AP-2F", "CCTV-NW")
  destination: string?          # where this port's cable run goes ("office_3_north_wall",
                                # "server_room_rack2_u15", "ceiling_ap_2nd_floor")
  cable_type: string?           # "cat6a_plenum", "cat6_riser", "om3_lc_duplex"
  cable_length_m: float?        # estimated or measured cable run length
  tested: bool?                 # has this run been certified/tested?
  test_result: string?          # "pass_cat6a", "pass_cat6", "fail_near_end_xt"
  connected_device: string?     # device ID at the other end (if known)
  # This is the structured cabling documentation that usually lives in a
  # spreadsheet or on a clipboard taped to the rack. Now it's in the graph,
  # queryable, and visualisable.

RackShelfSpec:
  shelf_type: string            # "fixed", "sliding", "vented", "cantilever"
  weight_capacity_kg: float?
  depth_mm: uint?               # usable depth
  width: string?                # "full_width", "half_width"

BlankingSpec:
  type: string                  # "solid", "brush", "vented", "hinged"
  # Blanking panels matter for airflow — an uncovered U creates a hot-air
  # recirculation path that reduces cooling efficiency. The graph can flag:
  # "3 empty U positions between your switch and server have no blanking
  # panels. This may cause hot air recirculation."

CableMgmtPanelSpec:
  type: string                  # "d_ring", "finger", "brush", "waterfall"
  capacity: uint?               # number of cables this panel can manage
  depth_mm: uint?               # cable management depth
```

**Patch panels in the routing graph**:

A patch panel is a passthrough device — data enters one side and exits the
other with no processing, no latency, and no bandwidth limitation (up to
the cable's rating). But it's critical for the graph because it defines
the **physical cable topology**. Without modelling the patch panel, the
graph shows "switch port 1 → server NIC" but doesn't know the cable route
goes through a patch panel at U6 and a 30-metre structured cable run
through the ceiling.

```
Switch (U5, port 1)
  → 0.3m patch cable (Cat6a, blue)
  → Patch panel (U6, port 1, labelled "Desk 3")
  → 30m structured cable run (Cat6a, plenum, through ceiling)
  → Wall plate (Office 3, north wall)
  → 2m patch cable (Cat6a)
  → Desktop NIC
```

The graph captures every hop. The cable inventory (from CableSpec) knows
every cable. The patch panel port mapping knows where each structured
cable run goes. Together: a complete wiring diagram, automatically
generated from the graph, printable for the clipboard on the rack.

**Audio patchbays**: In studio environments, TRS or bantam patchbays serve
the same function as Ethernet patch panels — they provide a normalised
connection point between studio equipment. A 48-point TRS patchbay
(24 in top row, 24 in bottom row) with half-normal or full-normal wiring
routes audio between the mixing desk, outboard gear, and the audio
interface. The same `PatchPanelSpec` model handles audio patchbays with
`port_type: "trs_6.35mm"` or `"trs_bantam"`.

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

**RamSpec** (DIMM modules — speed, timings, XMP profiles, RGB):

```yaml
RamSpec:
  type: string?                 # "ddr4", "ddr5", "lpddr5", "lpddr4x", "ddr3"
  form_factor: string?          # "dimm", "so_dimm", "lpcamm2", "soldered"
  capacity_gb: uint?            # per-module capacity
  modules: uint?                # number of modules in this kit
  total_capacity_gb: uint?      # total kit capacity

  # Speed and timings
  jedec_speed_mhz: uint?        # base JEDEC speed (e.g., 4800 for DDR5)
  xmp_profiles: XmpProfile[]?   # Intel XMP profiles
  expo_profiles: XmpProfile[]?  # AMD EXPO profiles
  active_speed_mhz: uint?       # currently running speed (reported by OS/BIOS)
  active_timings: RamTimings?   # currently active timings
  voltage_v: float?             # operating voltage (1.1V DDR5, 1.2V DDR4, 1.35V DDR4 XMP)

  # Physical
  height_mm: float?             # module height (low-profile matters for cooler clearance)
  heatspreader: bool?
  rgb: RgbSpec?                 # addressable RGB on the module (LED count, zones, protocol)
  rgb_protocol: string?         # "spd_hub" (DDR5 native), "vendor_usb" (Corsair iCUE),
                                # "vendor_smbus" (G.Skill, Kingston), "openrgb"

  # Identity
  spd: SpdData?                 # SPD EEPROM data (if readable)

XmpProfile:
  profile_number: uint          # 1, 2, 3 (XMP 3.0 supports 3 profiles + 2 user)
  name: string?                 # profile name ("XMP I", "EXPO I", "User 1")
  speed_mhz: uint               # advertised speed
  timings: RamTimings
  voltage_v: float              # required voltage
  verified_for: string?         # "intel", "amd", "both"

RamTimings:
  cl: uint                      # CAS latency
  trcd: uint                    # RAS to CAS delay
  trp: uint                     # row precharge time
  tras: uint                    # row active time
  # First four timings (CL-tRCD-tRP-tRAS) are the headline numbers.
  # Additional sub-timings are optional:
  trc: uint?
  trfc: uint?
  trrd_s: uint?
  trrd_l: uint?
  tfaw: uint?
  tcwl: uint?
  cr: uint?                     # command rate (1T or 2T)

SpdData:
  manufacturer: string?         # JEDEC manufacturer from SPD
  part_number: string?          # module part number from SPD
  serial_number: string?
  manufacturing_date: string?   # year-week
  revision: string?
  # SPD data is readable via i2c-tools (Linux), CPU-Z (Windows),
  # or dmidecode. It's the authoritative source for module identity.
```

**Why RAM matters for the routing graph**:

1. **Performance detection**: RAM running at JEDEC speed (4800 MHz) instead
   of XMP/EXPO speed (6000+ MHz) is leaving 20–30% memory bandwidth on the
   table. The agent can detect this via `dmidecode` or `/sys/devices/`
   and alert: "Your RAM supports XMP 6400 MHz but is running at JEDEC
   4800 MHz. Enable XMP in BIOS for better performance."

2. **Capacity and pressure**: The resource model (§2.7) already tracks
   `memory_mb` as a resource pool. RAM specs tell you the ceiling and
   whether it's upgradeable (`form_factor: "soldered"` = no upgrade path).

3. **RGB control**: DDR5 has native RGB control via the SPD hub (I2C on the
   DIMM). DDR4 uses vendor-specific protocols (Corsair iCUE over USB,
   G.Skill via SMBus). The routing graph models RAM RGB as an RGB endpoint
   (§2.8) driven through the appropriate protocol. The device database
   entry for a specific DIMM kit records its RGB protocol so the RGB
   compositor knows how to address it.

4. **Dual-channel/interleaving detection**: Two DIMMs in the right slots
   (A2+B2 on most boards) run in dual-channel. One DIMM or two in the
   same channel runs single-channel — half the bandwidth. The agent can
   detect this from `dmidecode` or sysfs and warn.

5. **Device database matching**: A Corsair Vengeance DDR5-6400 kit has
   a database entry with XMP profiles, RGB protocol, and module height.
   When the agent reads the SPD part number, it matches to the entry and
   knows the RGB protocol without user configuration.

**PsuSpec** (power supply — the source of all power rails in a PC):

```yaml
PsuSpec:
  wattage: uint                 # total rated wattage
  efficiency: string?           # "80plus_white", "80plus_bronze", "80plus_gold",
                                # "80plus_platinum", "80plus_titanium"
  modular: string?              # "non_modular", "semi_modular", "fully_modular"
  form_factor: string?          # "atx", "sfx", "sfx_l", "tfx", "flex_atx"
  fan_size_mm: uint?            # cooling fan diameter (120, 135, 140)
  fan_mode: string?             # "always_on", "semi_passive", "fanless"
  rails: PsuRail[]?             # output rails with capacity
  connectors: PsuConnector[]?   # available power connectors
  protections: string[]?        # ["ovp", "uvp", "ocp", "opp", "scp", "otp"]
  atx_version: string?          # "atx_2.x", "atx_3.0", "atx_3.1"
  pcie_gen5: bool?              # 12VHPWR / 12V-2x6 connector
  monitoring: PsuMonitoring?    # if the PSU reports telemetry

PsuRail:
  voltage_v: float              # 3.3, 5, 12, -12, 5VSB
  max_current_a: float          # maximum current on this rail
  max_power_w: float?           # maximum power on this rail
  regulation_percent: float?    # voltage regulation (±2%, ±5%)
  ripple_mv: float?             # maximum ripple

PsuConnector:
  type: string                  # "24pin_atx", "8pin_eps", "4pin_eps", "8pin_pcie",
                                # "6pin_pcie", "12vhpwr", "12v_2x6",
                                # "sata", "molex", "fdd"
  count: uint                   # how many of this connector

PsuMonitoring:
  protocol: string?             # "corsair_link", "evga_supernova", "seasonic_connect",
                                # "pmbus", "none"
  metrics: string[]?            # ["input_voltage", "output_voltage", "current",
                                # "power", "temperature", "fan_speed", "efficiency"]
  # Smart PSUs with monitoring are power measurement devices (§2.10).
  # A Corsair HX1200i reports per-rail voltage and current in real-time.
  # This feeds directly into the power model as `measured` quality data.
```

**CoolerSpec** (CPU coolers, AIOs, case fans, custom loop components):

```yaml
CoolerSpec:
  cooler_type: string?          # "air_tower", "aio_120", "aio_240", "aio_280",
                                # "aio_360", "custom_loop", "passive", "stock",
                                # "case_fan", "server_fan", "blower"
  socket_support: string[]?     # ["lga1700", "am5", "lga1200", "am4"]
  tdp_rated_w: uint?            # rated cooling capacity
  height_mm: float?             # total height (matters for case clearance)
  radiator_size_mm: Dimensions? # for AIO: { w, d, h }
  pump_speed_rpm: uint?         # for AIO/custom loop
  rgb: RgbSpec?                 # fan/block RGB

  # Fan specifications (applies to standalone fans, fans on coolers, server fans)
  fan: FanSpec?                 # detailed fan specs (null for fanless/passive coolers)

  # For custom loop components:
  loop_component: string?       # "pump", "reservoir", "radiator", "block", "fitting", "tubing"
  thread_size: string?          # "g1_4" (standard PC watercooling)
  flow_rate_lph: float?         # litres per hour at rated speed

FanSpec:
  count: uint?                  # number of fans (dual-tower coolers = 2, AIO = 1–3)
  size_mm: uint?                # fan diameter (40, 60, 80, 92, 120, 140, 200)
  thickness_mm: uint?           # fan thickness (15, 25, 38 — server fans often 38mm)
  rpm_range: { min: uint, max: uint }?
  noise_dba: float?             # rated noise at max speed
  noise_at_idle_dba: float?     # noise at minimum speed

  # Airflow and pressure
  airflow_cfm: float?           # cubic feet per minute (max)
  airflow_m3h: float?           # cubic metres per hour (max)
  static_pressure_mmh2o: float? # static pressure (mm H₂O) — matters for pushing
                                # air through drive cages, radiators, filters
  # High CFM + low pressure = good for open airflow (case exhaust)
  # Low CFM + high pressure = good for obstructed paths (radiator, drive cage)
  # Server fans typically have both high CFM AND high pressure.

  # Electrical
  voltage_v: float?             # operating voltage (typically 12V, some 5V)
  current_max_a: float?         # maximum current draw
  power_max_w: float?           # maximum power consumption
  connector: string?            # "4pin_pwm", "3pin_dc", "2pin", "molex",
                                # "server_6pin", "proprietary"
  pwm: bool?                    # PWM speed control (4-pin)
  tachometer: bool?             # RPM feedback signal
  hot_swap: bool?               # can be replaced while system is running (server fans)

  # Mechanical
  bearing: string?              # "sleeve", "ball", "dual_ball", "fluid_dynamic",
                                # "maglev", "rifle", "hydro"
  rated_life_hours: uint?       # expected lifespan (ball bearing ~70k hrs, sleeve ~30k hrs)
  direction: string?            # "intake", "exhaust" — when mounted in this orientation

  # Server-specific
  redundant: bool?              # part of a redundant fan group (N+1)
  fan_zone: string?             # which thermal zone this fan serves (see ThermalTopology)
```

**ThermalTopology** — which fans cool which components, and what happens
when a fan fails. Added to MotherboardSpec and CaseSpec:

```yaml
ThermalTopology:
  zones: ThermalZone[]
  redundancy: ThermalRedundancy?

ThermalZone:
  id: string                    # "cpu_zone", "drive_zone", "pcie_zone", "exhaust"
  name: string                  # "CPU / VRM area", "Drive cage", "PCIe card area"
  fans: string[]                # fan device IDs that serve this zone
  components: string[]          # device IDs of components cooled by this zone
  sensors: string[]?            # temperature sensor IDs monitoring this zone
  target_temp_c: float?         # target temperature for fan curve
  critical_temp_c: float?       # thermal throttle / shutdown threshold
  airflow_direction: string?    # "front_to_rear", "bottom_to_top", "rear_exhaust"

ThermalRedundancy:
  mode: string                  # "none", "n_plus_1", "n_plus_2", "full_redundancy"
  min_fans_operational: uint?   # minimum fans before degraded state
  action_on_fan_failure: string? # "increase_remaining", "throttle", "alert", "shutdown"
  # N+1: system can lose one fan and still cool adequately (remaining fans speed up)
  # Full: every fan has a dedicated backup
```

**Your server modelled**:

```yaml
# Thermal topology for the 4U server
thermal_topology:
  zones:
    - id: "drive_zone"
      name: "24-bay drive cage"
      fans: ["arctic_fan_1", "arctic_fan_2"]  # two fans push air through drive cage
      components: ["sas_backplane", "sas_expander_0", "sas_expander_1"]
      sensors: ["drive_cage_temp"]
      airflow_direction: "front_to_rear"
      # Arctic S12038-4K: 120×38mm, 4000 RPM, 84 CFM, 3.58 mmH₂O static pressure
      # High static pressure pushes air through 24 populated drive bays

    - id: "cpu_pcie_zone"
      name: "CPU, motherboard, and PCIe cards"
      fans: ["arctic_fan_3"]    # one fan behind CPU/PCIe area
      components: ["cpu_5900x", "x570s_mobo", "rx6600xt", "x540_10gbe",
                   "9300i_hba", "1660s_via_riser"]
      sensors: ["cpu_tdie", "vrm_temp", "chipset_temp"]
      airflow_direction: "front_to_rear"

    - id: "psu_zone"
      name: "PSU"
      fans: []                  # PSU has its own internal fan
      components: ["evga_1600t2"]

  redundancy:
    mode: "n_plus_1"            # with 3 fans, can lose 1 and still cool
    min_fans_operational: 2
    action_on_fan_failure: "increase_remaining"
    # If arctic_fan_2 dies, fan_1 and fan_3 ramp to compensate.
    # Drive zone has only 1 fan remaining — may thermal throttle under
    # heavy I/O. Alert the user.
```

**Bandwidth topology for your server** (the interesting part):

```
CPU: Ryzen 9 5900X (Zen 3, AM4)
├── PCIe Gen 4 x16 → RX 6600 XT (full bandwidth, 32 GB/s)
│   └── Display engine: 4 heads, 4 PLLs, RDNA2 VCN 3.0 (4 encode sessions)
│
├── PCIe Gen 4 x4 → X570S chipset (uplink: 8 GB/s) ← BOTTLENECK
│   ├── Chipset PCIe → X540-T2 10GbE dual (25 Gbps per port = 6.25 GB/s)
│   ├── Chipset PCIe x1 → [x1-to-x16 riser] → GTX 1660 Super
│   │   └── x1 Gen 3 = 1 GB/s — GPU runs but severely starved.
│   │       NVENC works fine for encoding (data is small).
│   │       Gaming/rendering: unusable at x1.
│   ├── Chipset M.2 x4 → [M.2-to-x16 adapter] → Broadcom 9300-8i
│   │   └── HBA is PCIe x8 card but adapter limits to x4 = 4 GB/s.
│   │       HBA drives 24-bay SAS backplane.
│   │       24× spinning drives at 250 MB/s each = 6 GB/s aggregate
│   │       but adapter limits to 4 GB/s. Mild bottleneck under full load.
│   ├── Chipset USB 3.0 (shared 5 Gbps)
│   ├── Chipset SATA (if any drives connected)
│   └── Chipset USB 2.0 / audio / etc.
│
│   Total chipset demand: 10GbE (6.25 GB/s) + HBA (4 GB/s) + 1660S (1 GB/s)
│                       + USB + SATA = ~11+ GB/s through 8 GB/s uplink
│                       ← OVERSUBSCRIBED. Under full 10GbE + storage load,
���                          the chipset uplink is the bottleneck.
│
├── Direct USB (if AM4 has any — varies by board)
└── Memory: 2× 32GB DDR4 (dual channel, slots A2+B2 presumably)
    └── If running at JEDEC 3200 MHz: 51.2 GB/s
        If XMP enabled (e.g., 3600 MHz): 57.6 GB/s
```

The Sankey diagram for this system would clearly show the chipset uplink as
the narrowest point between the CPU and all the storage + networking. The
router knows: "heavy NFS serving (10GbE saturated) will reduce available
bandwidth for the SAS array because they share the 8 GB/s chipset uplink."
```

**CaseSpec** (PC case — already in device-db.md for RGB/spatial, extended
here for the full PC model):

```yaml
CaseSpec:
  form_factor: string?          # "full_tower", "mid_tower", "mini_tower", "sff",
                                # "mini_itx", "desktop", "htpc", "open_frame", "test_bench"
  motherboard_support: string[]? # ["eatx", "atx", "matx", "mitx", "dtx"]
  psu_form_factor: string?      # "atx", "sfx", "sfx_l", "flex_atx", "tfx"
  psu_position: string?         # "bottom", "top", "rear"
  max_gpu_length_mm: uint?
  max_cpu_cooler_height_mm: uint?
  max_psu_length_mm: uint?

  drive_bays: DriveBay[]?
  fan_mounts: FanMount[]?       # available fan/radiator positions
  radiator_support: RadiatorMount[]? # where AIOs/radiators can go
  io_panel: IoPanel?            # front/top I/O panel

  airflow: string?              # "front_intake", "bottom_intake", "negative_pressure",
                                # "positive_pressure", "passive"
  dust_filters: string[]?       # ["front", "bottom", "top", "psu"]
  tempered_glass: string[]?     # which panels are glass (["left", "right", "front"])
  rgb: RgbSpec?                 # built-in case RGB (strips, fans, logo)

DriveBay:
  type: string                  # "3.5_internal", "2.5_internal", "5.25_external"
  count: uint
  hot_swap: bool?               # hot-swap cage

FanMount:
  position: string              # "front", "top", "rear", "bottom", "side"
  sizes_mm: uint[]              # supported fan sizes [120, 140]
  count: uint                   # how many fans in this position
  included_fan: string?         # device database ID of pre-installed fan

RadiatorMount:
  position: string              # "front", "top", "bottom", "side"
  max_radiator_mm: uint         # maximum radiator length (240, 280, 360, 420)
  max_thickness_mm: uint?       # maximum radiator + fan thickness

IoPanel:
  position: string              # "front_top", "front_bottom", "top"
  ports: PhysicalPort[]         # USB-A, USB-C, audio, etc. (reuses PhysicalPort from MotherboardSpec)
  # Front panel I/O connects to motherboard internal headers.
  # The internal topology traces: front USB-C → internal USB-C header →
  # motherboard USB controller → chipset/CPU. The full path is known.
```

**LaptopSpec** (laptop-specific topology and power behaviour):

Laptops are fundamentally different from desktops in how GPUs connect to
displays, how power state affects performance, and how thermal limits
constrain capability. The routing graph needs to model these differences
because they directly affect what pipelines are possible and at what quality.

```yaml
LaptopSpec:
  # --- GPU topology ---
  gpu_switching: GpuSwitchingSpec?  # how iGPU and dGPU interact
  display_mux: DisplayMuxSpec?      # hardware display MUX (if present)

  # --- Power state and performance ---
  power_states: LaptopPowerState[]  # different performance profiles based on power source
  charger: LaptopChargerSpec?       # expected charger and wattage effects

  # --- Thermal modes (vendor-specific) ---
  thermal_modes: LaptopThermalMode[]?  # vendor performance/silent/turbo modes

  # --- Physical (from device-db.md laptop library) ---
  screen: DisplaySpec?              # built-in display panel
  keyboard_backlight: bool?
  trackpad: bool?
  fingerprint: bool?
  ir_camera: bool?                  # Windows Hello IR
  thunderbolt_ports: uint?
  sd_card: string?                  # "sd", "microsd", "none"

GpuSwitchingSpec:
  type: string                  # "optimus" (NVIDIA), "enduro" (AMD), "apple_switching",
                                # "mux_switch", "discrete_only", "integrated_only"
  igpu: DeviceRef               # iGPU device in the graph
  dgpu: DeviceRef?              # dGPU device in the graph (null if integrated-only)
  render_offload: bool?         # dGPU renders, iGPU composites and displays (Optimus/Enduro)
  direct_output: bool?          # dGPU can drive external displays directly (without iGPU copy)
  # Optimus/Enduro: dGPU renders to framebuffer, iGPU copies to display → adds ~1 frame latency
  # Direct output: dGPU drives HDMI/DP directly → no copy latency, but can't use iGPU encode simultaneously
  output_routing: LaptopOutputRouting[]  # which GPU drives which display output

LaptopOutputRouting:
  output: string                # "internal_panel", "hdmi_1", "usb_c_1", "mini_dp_1"
  routed_via: string            # "igpu_only", "dgpu_only", "mux_switchable", "either"
  mux_state: string?            # if mux_switchable: current state ("igpu", "dgpu")
  bandwidth_impact: string?     # "full" (direct), "copy_overhead" (Optimus render offload)
  # Example: internal panel is typically igpu_only (or mux_switchable on gaming laptops)
  #          HDMI is typically dgpu_only (wired directly to dGPU)
  #          USB-C DP might be igpu on some, dgpu on others, or either

DisplayMuxSpec:
  present: bool                 # does this laptop have a hardware MUX?
  type: string?                 # "advanced_optimus" (NVIDIA), "mux_switch" (manual),
                                # "dynamic" (switches without reboot)
  switchable_outputs: string[]  # which outputs the MUX controls
  requires_reboot: bool?        # does switching require a reboot? (older MUX = yes)
  current_state: string?        # "igpu", "dgpu" (detected at runtime)
  # MUX matters because Optimus adds 1 frame of latency (iGPU copy).
  # With MUX on dGPU: internal panel is driven directly by dGPU = no copy latency.
  # Gaming laptops with MUX are meaningfully faster than without.

LaptopPowerState:
  source: string                # "ac_full", "ac_low_wattage", "battery", "usb_c_pd"
  charger_wattage_w: float?     # wattage of the connected charger (null = battery)
  cpu_tdp_w: float              # CPU TDP in this power state
  cpu_boost_w: float?           # CPU boost power limit
  gpu_tdp_w: float?             # dGPU TDP in this power state
  gpu_boost_w: float?           # dGPU boost power limit
  max_display_brightness: float? # brightness cap (some laptops dim on battery)
  max_refresh: float?           # display refresh cap (some drop to 60Hz on battery)
  encode_sessions: uint?        # NVENC/VCN session cap in this state (some throttle on battery)
  notes: string?

# Example: a gaming laptop with 140W charger
# power_states:
#   - source: "ac_full"
#     charger_wattage_w: 140
#     cpu_tdp_w: 45
#     cpu_boost_w: 65
#     gpu_tdp_w: 100
#     gpu_boost_w: 115
#     notes: "Full performance — all-core boost + GPU boost"
#
#   - source: "ac_low_wattage"
#     charger_wattage_w: 65        # user plugged in a 65W charger
#     cpu_tdp_w: 35
#     cpu_boost_w: 45
#     gpu_tdp_w: 60                 # GPU power-limited because charger can't supply full draw
#     notes: "Reduced performance — 65W charger limits total power budget"
#
#   - source: "battery"
#     cpu_tdp_w: 25
#     gpu_tdp_w: 40
#     max_display_brightness: 0.7    # 70% brightness cap
#     max_refresh: 60                # drops from 165Hz to 60Hz to save power
#     notes: "Battery mode — significant performance reduction"
#
#   - source: "usb_c_pd"
#     charger_wattage_w: 100         # USB-C PD charger
#     cpu_tdp_w: 35
#     gpu_tdp_w: 75
#     notes: "USB-C PD — better than battery, less than barrel jack"

LaptopChargerSpec:
  rated_wattage_w: float        # OEM charger wattage
  connector: string             # "barrel_dc", "usb_c_pd", "magsafe", "proprietary"
  voltage_v: float?             # charger voltage
  min_wattage_for_charging_w: float?  # below this: runs but doesn't charge
  min_wattage_for_full_perf_w: float?  # below this: performance is throttled
  usb_c_pd_capable: bool?       # can be charged via USB-C PD (even if OEM is barrel)
  usb_c_pd_max_w: float?        # max wattage via USB-C PD (may be less than barrel)

LaptopThermalMode:
  id: string                    # vendor mode ID
  name: string                  # "Silent", "Balanced", "Performance", "Turbo", "Beast Mode"
  vendor_api: string?           # how to switch ("acpi_platform_profile", "asus_armoury",
                                # "lenovo_legion", "dell_thermal", "hp_omen", "razer_synapse",
                                # "msi_center", "framework_ectool")
  cpu_tdp_w: float?             # CPU TDP in this mode
  gpu_tdp_w: float?             # GPU TDP in this mode
  fan_behavior: string?         # "silent", "standard", "aggressive", "full_speed"
  display_boost: bool?          # enables high-refresh on some laptops
  # Vendor thermal modes override the OS power profile. A laptop in
  # "Performance" mode with the OS set to "Power Saver" still uses the
  # vendor's aggressive fan curve and higher TDP limits.
```

**Why this matters for routing**:

1. **Charger-dependent encode capability**: A laptop on battery may throttle
   NVENC sessions or reduce GPU clock below what's needed for real-time H.265
   encoding. The router checks `LaptopPowerState` before placing encode jobs
   on a laptop's GPU. "Laptop is on battery — routing encode to controller's
   hardware encoder instead."

2. **Display output routing**: On an Optimus laptop, the HDMI port is
   wired to the dGPU — the iGPU can't drive it. If the user wants to capture
   the internal panel via the desktop agent, it goes through the iGPU. But
   capturing the HDMI output goes through the dGPU. The routing graph knows
   which GPU drives which output and can recommend: "Connect the capture
   card to HDMI (dGPU direct) for lowest latency."

3. **MUX state awareness**: With Advanced Optimus, the MUX state determines
   whether the internal panel has Optimus copy overhead or not. The agent
   detects MUX state and reports it. The router accounts for the ~1 frame
   additional latency when Optimus is copying through the iGPU.

4. **65W charger alert**: "Your laptop supports 140W but is connected via
   a 65W USB-C charger. GPU performance is limited to 60W. Use the OEM
   140W charger for full performance." The agent detects charger wattage
   via ACPI/platform driver and compares against `LaptopChargerSpec`.

5. **Thermal mode integration**: The vendor's thermal mode (ASUS Armoury,
   Lenovo Legion, Dell Thermal) maps to an Ozma power profile. When the
   intent switches to `gaming`, Ozma can switch the laptop's thermal mode
   to "Performance" via the vendor API — and switch back to "Silent" when
   the intent returns to `desktop`.

**CableSpec** (cables that affect signal quality — the invisible bottleneck):

```yaml
CableSpec:
  cable_type: string            # "hdmi", "dp", "usb_a_to_c", "usb_c_to_c",
                                # "usb_a_to_b", "ethernet_cat6", "ethernet_cat6a",
                                # "optical_fiber", "3.5mm_audio", "xlr", "sata",
                                # "thunderbolt_4", "usb4"
  length_m: float?
  version: string?              # "hdmi_2.1_ultra_high_speed", "dp_1.4_hbr3",
                                # "usb3_gen2", "usb4_40gbps", "cat6a"
  max_bandwidth_gbps: float?    # rated bandwidth
  certified: bool?              # officially certified (HDMI Premium, USB-IF, etc.)
  active: bool?                 # active cable (has signal repeater/retimer)
  optical: bool?                # optical cable (fiber, not copper)
  gauge_awg: uint?              # wire gauge (affects power delivery and signal quality)
  shielded: bool?
  max_power_delivery_w: float?  # for USB-C: 60W (passive) or 240W (EPR, active/emarked)
  emark: bool?                  # USB-C electronically marked cable

  # Cables degrade signals. A 3m passive HDMI cable may not support 4K120.
  # A USB-C cable rated for USB 2.0 limits a USB 3.2 port to 480 Mbps.
  # The router can detect this (device reports USB 2.0 speed on a USB 3.0 port)
  # and recommend: "Your USB-C cable is limiting this connection to USB 2.0.
  # Use a USB 3.2 Gen 2 certified cable for 10 Gbps."
  
  # Cable detection: the router can't directly identify a cable, but it can
  # infer cable quality from the gap between port capability and measured speed.
  # A USB 3.0 port measuring USB 2.0 throughput → cable or hub bottleneck.
  # An HDMI 2.1 port that can't sustain 4K120 → cable bandwidth limit.
```

**Why cables matter**: Cables are the most common undiagnosed performance
bottleneck. A USB-C cable that came with a phone charger is typically USB
2.0 — plugging it into a USB 3.2 Gen 2 port gives you 480 Mbps instead of
10 Gbps, and no error message tells you why. HDMI cables that don't support
the full bandwidth of HDMI 2.1 cause resolution or refresh rate drops. The
router detects the symptom (measured speed < port capability) and infers the
cause: "The cable between your PC and capture card appears to be limiting
bandwidth. Check that you're using a USB 3.x cable, not a USB 2.0 cable."

Cables don't have device database entries in the traditional sense (they
don't have VID/PIDs). But USB-C emarked cables report their capabilities via
the USB PD CC line, which the agent can read. HDMI 2.1 cables with
48 Gbps certification are detectable via EDID/SCDC. For everything else,
the router infers cable quality from the measured vs expected performance gap.

**AdapterSpec** (adapters, risers, and converters that bridge interface types):

Adapters change the physical form factor and often constrain the electrical
capabilities. They sit in the graph as devices with an input port and an
output port (or multiple), with the adapter's constraints applied as
internal link properties.

```yaml
AdapterSpec:
  adapter_type: string          # type of adaptation (see table below)
  input: AdapterPort            # what this adapter plugs into
  output: AdapterPort           # what this adapter exposes
  signal_conversion: bool       # does this adapter convert the signal? (active)
                                # or just change the physical connector? (passive)
  max_generation: uint?         # maximum PCIe generation supported (for PCIe adapters)
  max_lanes: uint?              # maximum electrical lanes passed through
  bandwidth_limit_gbps: float?  # if the adapter constrains bandwidth below the interface max
  signal_integrity: string?     # "transparent", "minor_degradation", "retimed"
  power_passthrough: bool?      # does it pass power (e.g., USB-C PD passthrough)
  power_limit_w: float?         # maximum power passthrough if limited
  active: bool?                 # active adapter (has electronics, may need power)
  chipset: string?              # conversion chipset (for active adapters)
  additional_ports: PhysicalPort[]?  # extra ports on the adapter (e.g., USB hub on a USB-C dock adapter)

AdapterPort:
  connector: string             # physical connector type (from §15 connector table)
  interface: string             # electrical interface ("pcie_x4", "usb3_10gbps",
                                # "dp_1.4", "hdmi_2.0", "m2_m_key", "sata")
  gender: string?               # "male" (plug), "female" (socket)
```

**Common adapter types and their constraints**:

| Adapter | Input | Output | Electrical | Key constraint |
|---------|-------|--------|-----------|---------------|
| PCIe riser (vertical GPU mount) | x16 male | x16 female | x16 passthrough | Signal integrity — Gen 4/5 need shielded riser. Cheap risers may force Gen 3 negotiation. |
| M.2 to PCIe x16 card | M.2 M-key | x16 physical (x4 electrical) | x4 passthrough | Looks like x16 slot but is x4. GPU will fit but run at x4 speed. |
| PCIe x1 to x16 riser (mining style) | x1 male | x16 female | x1 only | USB cable between halves — x1 bandwidth (1 GB/s Gen 3). For capture cards, not GPUs. |
| M.2 to U.2 | M.2 M-key | U.2 (SFF-8639) | x4 passthrough | For enterprise NVMe drives. Transparent. |
| M.2 to SATA | M.2 B+M key | SATA data + power | SATA only | Only works with M.2 SATA drives, not NVMe. Common confusion. |
| USB-C to HDMI (passive) | USB-C male | HDMI female | DP Alt Mode → HDMI | Limited to HDMI 2.0 (18 Gbps) on most passive adapters. No 4K120. |
| USB-C to HDMI (active) | USB-C male | HDMI female | DP → HDMI conversion | Active chipset enables HDMI 2.1. Some support 4K120. |
| USB-C to DP | USB-C male | DP female | DP Alt Mode passthrough | Transparent if enough DP lanes allocated. 2-lane = half bandwidth. |
| USB-C multiport (hub adapter) | USB-C male | HDMI + USB-A + Ethernet + PD | Splits USB-C bandwidth | Video + USB data + Ethernet share the USB-C link. Internal topology matters (§15 DockSpec). |
| DP to HDMI (passive) | DP male | HDMI female | DP++ → HDMI | DP++ single-link only — max 4K30 or 1080p120. |
| DP to HDMI (active) | DP male | HDMI female | Signal conversion | Active chipset. Can do 4K60+ depending on chipset. |
| HDMI to DP | HDMI male | DP female | Signal conversion | Always active — HDMI→DP requires a converter chip. No passive option. |
| HDMI to VGA | HDMI male | VGA female | Digital → analog | Active — DAC conversion. Max 1080p60. Audio lost (VGA is video only). |
| DVI to HDMI | DVI male | HDMI female | Pin-compatible | Passive — DVI-D and HDMI are electrically compatible. No audio over DVI. |
| Thunderbolt dock | TB male | Multiple | PCIe + DP tunneled | Full DockSpec (§15) — internal topology, USB hub chipset, DP MST, etc. |
| USB-A to USB-C | USB-A male | USB-C female | USB data + limited power | USB 3.x data. No DP Alt Mode, no PD negotiation, max 5V/0.9A power. |

**Adapters in the routing graph**: An adapter is modelled as a device with
an input port (what it plugs into) and one or more output ports (what it
exposes). The internal link between them carries the adapter's constraints:

```
GPU → [PCIe riser cable] → Motherboard x16 slot
      ↑
      Device: "pcie_riser_1"
      Input port: x16 female (from GPU)
      Output port: x16 male (to motherboard slot)
      Internal link: x16 passthrough, Gen 4, signal_integrity: "minor_degradation"
```

```
NVMe SSD → [M.2 to PCIe x16 adapter card] → Motherboard x16 slot
            ↑
            Device: "m2_to_pcie_adapter_1"
            Input port: M.2 M-key (from SSD)
            Output port: x16 physical / x4 electrical (to slot)
            Internal link: x4 passthrough, Gen 4
            # Router knows: device in this slot is x4, not x16
```

```
Laptop USB-C → [USB-C to HDMI adapter] → Monitor HDMI input
               ���
               Device: "usb_c_hdmi_adapter_1"
               Input port: USB-C male (DP Alt Mode, 2 DP lanes)
               Output port: HDMI female
               Internal link: DP→HDMI conversion, max 4K60, HDMI 2.0
               chipset: "Parade PS176"
               # Router knows: this path can't do 4K120 — adapter limits it
```

The router traces through adapters like any other device in the path. When
it calculates pipeline bandwidth, the adapter's constraints are applied at
its internal link. "Your USB-C to HDMI adapter limits this path to HDMI 2.0
(4K60 max). For 4K120, use a direct DisplayPort connection or an active
USB-C to HDMI 2.1 adapter."

**Detection**: Some adapters are visible to the OS:
- USB-C display adapters appear as USB devices with VID/PID → device database match
- PCIe adapters are transparent (the device behind them is visible, the adapter isn't)
- Risers are invisible — detected by inference (Gen 4 device negotiating Gen 3 = possible riser issue)

For invisible adapters, the user can add them to their setup in the layout
editor, or the router infers their presence from performance discrepancies.

**Putting it together — a complete virtual PC in the device database**:

Every component of a PC can now be modelled:

```
PC "Gaming Rig"
├── Case: NZXT H510 (CaseSpec — fan mounts, drive bays, airflow, RGB)
├── Motherboard: ASRock B760I (MotherboardSpec — physical ports, internal topology)
│   ├── CPU: Intel i5-13600K (CpuSpec — cores, iGPU/Quick Sync, PCIe lanes)
│   │   └── iGPU: Intel UHD 770 (IgpuSpec — Quick Sync encode/decode)
│   ├── Chipset: Intel B760 (ChipsetSpec — DMI uplink, USB controllers, lane sharing)
│   ├── RAM Slot A2: Corsair Vengeance DDR5-6400 16GB (RamSpec — XMP, timings, RGB)
│   ├── RAM Slot B2: Corsair Vengeance DDR5-6400 16GB (RamSpec — dual channel)
│   ├── M.2 Slot 1: Samsung 990 Pro 2TB (StorageSpec — NVMe Gen 4, IOPS)
│   ├── M.2 Slot 2: WD SN770 1TB (StorageSpec — shares lanes with SATA!)
│   └── PCIe x16: NVIDIA RTX 4070 (GpuSpec — NVENC, display engine, 4 outputs)
│       ├── Display Engine (DisplayEngineSpec — heads, PLLs, link bandwidth)
│       ├── NVENC (GpuCodecCapability — 12 encode sessions, AV1/H.265/H.264)
│       └── NVDEC (GpuCodecCapability — unlimited decode sessions)
├── PSU: Corsair RM850x (PsuSpec — 850W, 80+ Gold, 12VHPWR, Corsair Link monitoring)
├── CPU Cooler: Noctua NH-D15 (CoolerSpec — air tower, 165mm height, dual 140mm fans)
├── Case Fans: 2× Noctua NF-A14 (CoolerSpec — 140mm, 1500 RPM, 24.6 dBA)
├── Cables:
│   ├── HDMI 2.1 to Monitor (CableSpec — 48 Gbps, certified, 2m)
│   ├── USB-C to Capture Card (CableSpec — USB 3.2 Gen 2, emarked, 0.5m)
│   └── Cat6a to Switch (CableSpec — 2.5 Gbps capable, shielded, 3m)
└── Peripherals: [referenced by device ID, positioned via PhysicalLocation]
```

Every component has a device database entry. Every connection between
components traces through the internal topology. The router knows the full
path from any device to the CPU, including every bottleneck.

**MotherboardSpec** (the most important compound device — maps every physical
port to its internal controller):

```yaml
MotherboardSpec:
  form_factor: string?          # "atx", "matx", "mitx", "dtx", "eatx", "sff",
                                # "laptop", "sbc" (Pi, OPi), "embedded"
  chipset_id: string?           # device database ref for the chipset
  cpu_socket: string?           # "lga1700", "am5", "lga4677", "bga" (soldered)
  bios_type: string?            # "uefi", "legacy", "coreboot", "uboot"
  bios: BiosDatabase?           # BIOS version history, CPU support, known issues
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
| `barrel_dc` | power | DC barrel jack (see PowerConnectorSpec for size/polarity) |
| `iec_c14` | power | IEC 60320 C14 inlet (C13 cable, 10A, most PC PSUs + monitors) |
| `iec_c20` | power | IEC 60320 C20 inlet (C19 cable, 16A, servers + UPS) |
| `iec_c8` | power | IEC 60320 C8 inlet (C7 "figure-8" cable, 2.5A, small devices) |
| `iec_c6` | power | IEC 60320 C6 inlet (C5 "clover/mickey" cable, 2.5A, laptop PSUs) |
| `iec_c18` | power | IEC 60320 C18 inlet (C17 cable, 10A, hot conditions) |
| `nema_5_15` | power | NEMA 5-15 (US standard wall plug, 120V/15A) |
| `type_g` | power | BS 1363 (UK wall plug, 230V/13A) |
| `type_c_euro` | power | CEE 7/16 Europlug (230V/2.5A) |
| `type_f_schuko` | power | CEE 7/4 Schuko (230V/16A) |
| `anderson_pp` | power | Anderson Powerpole (DC, various ratings) |
| `xt60` | power | XT60 (DC, 60A, hobby/solar/battery) |
| `terminal_block` | power | Screw terminal (bare wire, various ratings) |
| `sfp` | data | SFP cage (1 Gbps — SX/LX/T copper) |
| `sfp_plus` | data | SFP+ cage (10 Gbps — SR/LR/DAC) |
| `sfp28` | data | SFP28 cage (25 Gbps) |
| `qsfp_plus` | data | QSFP+ cage (40 Gbps — 4×10G) |
| `qsfp28` | data | QSFP28 cage (100 Gbps — 4×25G) |
| `qsfp_dd` | data | QSFP-DD cage (200/400 Gbps) |
| `osfp` | data | OSFP cage (400/800 Gbps) |
| `lc_duplex` | data | LC duplex fiber connector (most common SFP fiber) |
| `sc_duplex` | data | SC duplex fiber connector (older, common in premises) |
| `mpo_mtp` | data | MPO/MTP multi-fiber connector (12/24 fiber, data centres) |
| `st` | data | ST bayonet fiber connector (legacy) |
| `firewire_400` | data, (power 12V/1.5A) | IEEE 1394a 6-pin (powered) or 4-pin (unpowered) |
| `firewire_800` | data, (power 12V/1.5A) | IEEE 1394b 9-pin (bilingual — accepts 400 + 800) |
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
  slot_type: string             # "pcie", "m2", "dimm", "u2"
  physical_size: string?        # physical slot size ("x16", "x8", "x4", "x1",
                                # "m_key_2280", "m_key_2242", "e_key_2230",
                                # "dimm_288pin", "so_dimm_260pin")
  electrical_lanes: uint?       # actual electrical lanes wired (may be < physical size)
                                # An x16 physical slot with 4 electrical lanes = x4 card works,
                                # x16 card fits but runs at x4 speed.
  source: string                # "cpu" or "chipset" — where the lanes/channel comes from
  generation: uint?             # PCIe generation (3, 4, 5) — for PCIe slots
  shared_with: string[]?        # other slots/ports that share lanes with this one
  bifurcation: BifurcationConfig?  # if this slot can be split
  position: PortPosition        # physical location on the board
  populated: string?            # device database ID of installed device (if known)
  reinforced: bool?             # metal-reinforced slot (for heavy GPUs)

  # DIMM-specific
  memory_channel: string?       # which memory channel ("A", "B", "C", "D")
  rank_in_channel: uint?        # position within the channel (0 = preferred for single-DIMM)
  daisy_chain_position: string? # "near" (closer to CPU, preferred) or "far"

BifurcationConfig:
  supported: bool               # can this slot be bifurcated in BIOS?
  options: string[]?            # ["x16", "x8_x8", "x4_x4_x4_x4"]
  current: string?              # currently active bifurcation mode
  requires_bios: bool?          # requires BIOS setting change (not runtime)
```

**Physical size vs electrical lanes** — the most common source of confusion:

| Scenario | Physical | Electrical | Result |
|----------|----------|-----------|--------|
| GPU in primary x16 slot | x16 | x16 | Full bandwidth ✓ |
| GPU in second x16 slot (many boards) | x16 | x4 | 1/4 bandwidth — GPU works but starved |
| x1 WiFi card in x16 slot | x16 | x16 | Works fine — card uses 1 lane, slot wastes 15 |
| NVMe adapter in x4 slot | x4 | x4 | Full bandwidth ✓ |
| Capture card in x4 slot | x4 | x1 (surprise!) | Bottlenecked — some cheap boards do this |
| M.2 in "M key" slot | M key | x2 | Some budget boards wire M.2 at x2 not x4 |

The router detects this via `lspci -vv` (reports negotiated link width) and
compares against the device's rated lane requirement from the device database.
If a PCIe x4 capture card is running at x1: "Your capture card in `pcie_x4_2`
is running at x1 speed. This slot has only 1 electrical lane despite being
physically x4. Move the card to `pcie_x16_1` for full x4 bandwidth."

**DIMM slot topology** — dual channel requires correct slot population:

```yaml
# Example: 4-DIMM DDR5 motherboard memory topology
DimmTopology:
  channels: MemoryChannel[]
  interleaving_rules: InterleavingRule[]

MemoryChannel:
  id: string                    # "A", "B"
  slots: DimmSlot[]
  max_capacity_gb: uint?        # per-channel maximum
  max_speed_with_all_populated_mhz: uint?  # speed drops when all slots filled

DimmSlot:
  id: string                    # "A1", "A2", "B1", "B2"
  channel: string               # "A", "B"
  position: string              # "near" or "far" (relative to CPU)
  daisy_chain: bool?            # daisy-chain topology (affects signal integrity)
  t_topology: bool?             # T-topology (equal trace length to both slots)
  preferred_single: bool        # populate this slot first for single-DIMM-per-channel
                                # (typically A2 and B2 on most boards)

InterleavingRule:
  population: string            # "1_dimm", "2_dimm_dual", "2_dimm_single",
                                # "3_dimm", "4_dimm"
  slots: string[]               # which slots to populate
  mode: string                  # "single_channel", "dual_channel", "quad_channel",
                                # "flex_mode"
  bandwidth_factor: float       # 1.0 = single, 2.0 = dual, 4.0 = quad
  notes: string?

# Typical 2-channel board (most consumer):
#
# For dual-channel: populate A2 + B2 (both "near" slots)
# Common mistake: populate A1 + A2 (same channel = single-channel, half bandwidth)
#
# interleaving_rules:
#   - population: "1_dimm"
#     slots: ["A2"]
#     mode: "single_channel"
#     bandwidth_factor: 1.0
#     notes: "Single DIMM — use A2 (near slot, best signal integrity)"
#
#   - population: "2_dimm_dual"
#     slots: ["A2", "B2"]
#     mode: "dual_channel"
#     bandwidth_factor: 2.0
#     notes: "Optimal 2-DIMM — dual channel, near slots"
#
#   - population: "2_dimm_single"
#     slots: ["A1", "A2"]
#     mode: "single_channel"
#     bandwidth_factor: 1.0
#     notes: "WRONG — both DIMMs in channel A, single-channel only"
#
#   - population: "4_dimm"
#     slots: ["A1", "A2", "B1", "B2"]
#     mode: "dual_channel"
#     bandwidth_factor: 2.0
#     notes: "All slots populated — dual channel but may downclock.
#             DDR5: typically drops from 6400 to 5600 with 4 DIMMs."
```

The agent detects the current population via `dmidecode -t memory` (which
slots have DIMMs, their capacity, speed, and part number). Combined with the
motherboard's `DimmTopology` from the device database, the router can detect:

- **Single-channel when dual is possible**: "You have 2 DIMMs in slots A1
  and A2 (same channel). Move one to B2 for dual-channel — double your
  memory bandwidth."
- **Wrong slots for 1 DIMM**: "Your single DIMM is in A1 (far slot). Move
  it to A2 (near slot) for better signal integrity and potential speed gain."
- **4 DIMMs causing speed drop**: "All 4 DIMM slots populated — memory
  running at 5600 MHz instead of XMP 6400 MHz. This is normal with 4 DIMMs
  on this board. For maximum speed, use 2× 32GB instead of 4× 16GB."
- **Mismatched DIMMs**: "Slot A2 has DDR5-6400 CL32, slot B2 has DDR5-5200
  CL40 — both running at the slower speed. Match your DIMMs for best
  performance."
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

**BiosDatabase** — BIOS/UEFI version history, CPU support, upstream
firmware, and known issues per revision:

```yaml
BiosDatabase:
  vendor: string?               # "ami", "phoenix", "insyde", "coreboot", "american_megatrends"
  current_version: string?      # detected running BIOS version (from dmidecode/SMBIOS)
  current_date: string?         # detected BIOS date
  versions: BiosVersion[]       # complete version history
  cpu_support: CpuSupportList   # which CPUs are supported at which BIOS version
  upstream: UpstreamFirmware?   # AGESA, microcode, FSP, ME version embedded in each BIOS

BiosVersion:
  version: string               # BIOS version string (e.g., "7.03", "F37", "2803")
  date: string?                 # release date (ISO)
  upstream_version: UpstreamVersion?  # AGESA/microcode/FSP version in this BIOS
  changelog: string[]?          # vendor release notes (often terse/unhelpful)
  cpu_support_added: string[]?  # CPUs added in this version (device database CPU IDs)
  known_issues: BiosIssue[]?    # known bugs/problems in this version
  known_fixes: string[]?        # issues fixed in this version (references BiosIssue.id)
  recommended: bool?            # is this the vendor-recommended version?
  beta: bool?                   # beta/test release
  download_url: string?         # where to get this BIOS update
  recovery_only: bool?          # can only be flashed via recovery (e.g., USB BIOS flashback)

BiosIssue:
  id: string                    # unique issue identifier
  severity: string              # "critical", "major", "minor", "cosmetic"
  category: string              # "usb", "pcie", "memory", "boot", "stability", "security",
                                # "sleep", "power", "display", "audio", "network", "tpm",
                                # "virtualization", "nvme", "thunderbolt", "bluetooth"
  summary: string               # one-line description
  description: string?          # detailed description
  affected_hardware: string[]?  # which components are affected (CPU models, USB devices, etc.)
  workaround: string?           # known workaround if one exists
  fixed_in: string?             # BIOS version that fixes this issue (null = still unfixed)
  upstream_bug: string?         # upstream issue reference (AGESA, microcode, chipset driver)
  source: string[]?             # where this info came from ("vendor_notes", "community",
                                # "kernel_bugzilla", "reddit", "forum")
  affects_ozma: bool?           # does this issue specifically impact Ozma functionality?
  ozma_impact: string?          # how it affects Ozma ("usb_dropout", "xhci_crash",
                                # "s3_resume_fail", "pcie_link_train_fail")

CpuSupportList:
  initial_cpus: string[]        # CPUs supported from launch BIOS
  updates: CpuSupportUpdate[]   # CPUs added per BIOS version

CpuSupportUpdate:
  min_bios_version: string      # minimum BIOS version required
  cpus_added: string[]          # device database CPU IDs
  notes: string?                # e.g., "Requires AGESA 1.0.0.7 or later"

UpstreamFirmware:
  # The BIOS embeds upstream firmware blobs that determine much of the
  # platform's actual behaviour. Tracking these separately from the BIOS
  # version is critical because:
  # - The same AGESA version may appear in multiple BIOS versions
  # - An AGESA bug affects all boards using that AGESA, regardless of vendor
  # - Microcode updates fix CPU-level bugs independently of BIOS features
  type: UpstreamType            # which upstream firmware
  components: UpstreamComponent[]

UpstreamType: enum
  amd_agesa                     # AMD Generic Encapsulated Software Architecture
  intel_fsp                     # Intel Firmware Support Package
  intel_me                      # Intel Management Engine
  intel_microcode               # Intel CPU microcode
  amd_microcode                 # AMD CPU microcode
  arm_trusted_firmware          # ARM TF-A (for ARM SBCs)
  coreboot_payload              # coreboot payload version

UpstreamComponent:
  name: string                  # "agesa", "microcode", "me_firmware", "fsp"
  version: string               # version string (e.g., "ComboAM5PI 1.2.0.2",
                                # "0x0A704104", "16.1.30.2307")
  known_issues: UpstreamIssue[]?
  known_fixes: string[]?

UpstreamIssue:
  id: string                    # e.g., "agesa-usb-dropout-am5", "intel-mc-downfall"
  severity: string
  summary: string
  description: string?
  affected_platforms: string[]  # which chipsets/CPUs are affected
  fixed_in_version: string?     # upstream version that fixes it
  cve: string?                  # CVE ID if security-related
  source: string[]?
```

**Why this matters — real-world examples**:

**AMD AM5 USB dropouts (AGESA issue)**:
Early AMD AM5 boards (X670E, B650) had widespread USB disconnect issues —
devices would randomly drop and reconnect. This was an AGESA bug, not a
board-specific bug. It affected ALL AM5 boards until AGESA ComboAM5PI
1.0.0.7b. The fix rolled out over months as each board vendor released
BIOS updates containing the new AGESA.

```yaml
# Example issue in the database:
- id: "agesa-am5-usb-dropout-2022"
  severity: "critical"
  category: "usb"
  summary: "USB devices randomly disconnect and reconnect on AM5 platforms"
  affected_hardware: ["amd-x670e-chipset", "amd-b650-chipset"]
  workaround: "Disable USB selective suspend; avoid USB hubs on affected ports"
  fixed_in: null  # fixed per-board when BIOS with AGESA 1.0.0.7b was released
  upstream_bug: "agesa-combo-am5pi-pre-1.0.0.7b"
  affects_ozma: true
  ozma_impact: "usb_dropout — capture cards and nodes lose connection intermittently"
```

The agent detects the running BIOS version via `dmidecode -t bios`. The
device database entry for the motherboard contains the BIOS version history
with AGESA versions. The system checks:

1. Is the running BIOS version affected by any known issues?
2. Is there a newer BIOS that fixes known issues?
3. Does the running AGESA version have known upstream bugs?
4. Is the installed CPU supported by the running BIOS version?

**Alerts the user can see**:

- "Your ASRock B650E Taichi is running BIOS 1.08 (AGESA ComboAM5PI 1.0.0.6).
  This version has a known USB disconnect bug that affects capture cards.
  **Update to BIOS 3.08+ (AGESA 1.0.0.7b)** to fix USB stability."

- "Your BIOS (version F31) does not support the Ryzen 9 7950X3D. Minimum
  required: F37. **Update BIOS before installing this CPU** or it will not
  POST."

- "Intel microcode 0x0A704104 addresses the Downfall vulnerability (CVE-2022-40982).
  Your current microcode is 0x0A704103. **Update BIOS** for the security fix."

- "Your board's BIOS is 18 months old. 3 newer versions are available with
  stability improvements and new CPU support."

**Intel Management Engine version tracking**:

Intel ME is a separate firmware that runs on the chipset. It has its own
vulnerabilities (e.g., SA-00086, SA-00125) that are patched via BIOS
updates containing newer ME firmware. The database tracks ME versions per
BIOS version so the system can flag: "Your Intel ME version (11.8.50) is
affected by SA-00086. Update BIOS to version X for ME 11.8.93 which fixes
this."

**Discovery**:

| Data | Linux | Windows | macOS |
|------|-------|---------|-------|
| BIOS version/date | `dmidecode -t bios` | WMI `Win32_BIOS` | `system_profiler SPHardwareDataType` |
| BIOS vendor | `dmidecode -t bios` | WMI | N/A (Apple controls firmware) |
| CPU microcode | `/proc/cpuinfo` (`microcode` field) | `CPUID` instruction | N/A |
| Intel ME version | `mei-amt-check`, `lsmod \| grep mei` | Intel ME driver | N/A |
| AGESA version | Not directly exposed; inferred from BIOS version + database | Same | N/A |
| Board model | `dmidecode -t baseboard` | WMI `Win32_BaseBoard` | N/A |

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

### 15.12 Pinouts

Cables, connectors, and modular PSU cables have specific pin assignments
that determine compatibility and — in the case of power cables — safety.
Using a modular cable from one PSU brand on a different brand's PSU can
destroy components because the 12V and ground pins may be in different
positions. The device database stores pinout data to enable compatibility
checking and safety warnings.

**PinoutSpec** (added to CableSpec, PsuSpec, and any connector-bearing device):

```yaml
PinoutSpec:
  connector_type: string        # physical connector ("24pin_atx", "8pin_eps",
                                # "8pin_pcie", "6pin_pcie", "12vhpwr", "12v_2x6",
                                # "sata_power", "molex_4pin", "fdd_4pin",
                                # "usb_a", "usb_c", "hdmi_a", "dp_20pin",
                                # "rj45", "xlr_3pin", "trs_3.5mm", "din5_midi")
  pin_count: uint               # total pins
  pins: PinAssignment[]         # per-pin assignment
  keying: string?               # physical keying type ("polarised", "keyed_notch",
                                # "asymmetric", "colour_coded", "none")
  standard: string?             # defining standard ("atx_2.x", "atx_3.0", "usb_2.0",
                                # "iec_60320", "eia_568b")
  vendor_specific: bool         # true if pinout is proprietary (modular PSU cables!)
  compatible_with: string[]?    # other pinout IDs this is compatible with

PinAssignment:
  pin: uint                     # pin number (per standard pin numbering)
  name: string                  # signal name ("12V", "GND", "5V", "3.3V", "PS_ON",
                                # "D+", "D-", "CC1", "SBU1", "TX1+", etc.)
  voltage_v: float?             # nominal voltage on this pin (for power pins)
  max_current_a: float?         # maximum current rating
  signal_type: string?          # "power", "ground", "data", "control", "sense",
                                # "reserved", "no_connect"
  notes: string?
```

**Standard pinouts** (reference — these are universal):

The database includes canonical pinouts for every standard connector. These
never change — they're defined by specifications:

| Connector | Standard | Pins | Notes |
|-----------|----------|------|-------|
| ATX 24-pin | ATX/EPS12V | 24 | Main motherboard power. Universal across all PSUs. |
| EPS 8-pin (4+4) | EPS12V | 8 | CPU power. Universal. |
| PCIe 8-pin (6+2) | PCIe CEM | 8 | GPU power. 150W. Universal. |
| PCIe 6-pin | PCIe CEM | 6 | GPU power. 75W. Universal. |
| 12VHPWR | ATX 3.0 | 16 | GPU power. 600W. Sense pins determine power level. |
| 12V-2x6 | ATX 3.1 | 12 | Revised 12VHPWR. Same function, improved connector. |
| SATA power | Serial ATA | 15 | 3.3V + 5V + 12V. Universal. |
| Molex 4-pin | AMP MATE-N-LOK | 4 | 5V + 12V. Legacy. Universal. |
| USB-A | USB 2.0/3.x | 4/9 | Universal. |
| USB-C | USB Type-C | 24 | Universal. CC pins for PD/Alt Mode negotiation. |
| HDMI Type A | HDMI 1.x/2.x | 19 | Universal. |
| DisplayPort | DP 1.x/2.x | 20 | Universal. |
| RJ-45 | TIA/EIA-568B | 8 | T568A or T568B wiring. Crossover vs straight. |
| XLR 3-pin | IEC 61076-2-011 | 3 | Pin 1=GND, 2=hot, 3=cold. Universal. |
| TRS 3.5mm | IEC 60603-11 | 3/4 | Tip=L, Ring=R, Sleeve=GND. TRRS: Ring2=Mic. |
| DIN 5-pin MIDI | IEC 60268-3 | 5 | Pins 4+5 = data. Universal. |

Standard pinouts are canonical entries in the device database — every ATX
24-pin connector has the same pin assignments regardless of manufacturer.

**Modular PSU cables — the dangerous case**:

Modular PSU cables (the detachable cables between the PSU and components)
use proprietary pinouts on the **PSU side** of the cable. The component
side is standard (SATA, PCIe 8-pin, etc.), but the PSU-side connector
varies by manufacturer and sometimes by model line within the same
manufacturer.

```yaml
ModularPsuPinout:
  psu_side: PinoutSpec          # the PSU-side connector (PROPRIETARY)
  component_side: PinoutSpec    # the component-side connector (STANDARD)
  psu_family: string            # which PSU family this cable works with
  compatible_models: string[]   # specific PSU models confirmed compatible
  incompatible_models: string[]? # models confirmed INCOMPATIBLE (danger!)
  # WARNING: using a cable from one PSU family with a different PSU can
  # send 12V to 5V/3.3V rails and DESTROY components.

# Example compatibility groups:
#
# | PSU Family | Compatible cables between models | PSU-side connector |
# |------------|--------------------------------|-------------------|
# | Corsair Type 4 | RM/RMx/HX/AX (2017+) | Corsair proprietary 18-pin |
# | Corsair Type 3 | RM/HX/AX (2013–2016) | DIFFERENT from Type 4! |
# | EVGA G2/G3/P2/T2 | All interchangeable | EVGA proprietary |
# | Seasonic Focus/Prime | All interchangeable | Seasonic proprietary |
# | be quiet! | Straight Power, Dark Power | be quiet! proprietary |
# | Corsair Type 4 ↔ EVGA | INCOMPATIBLE — DANGER | Different pinouts |
# | Corsair Type 3 ↔ Type 4 | INCOMPATIBLE — DANGER | Same brand, different pinout! |
```

**Safety checking**: If the user tells the system which PSU they have
(device database match or manual entry), and which modular cables they're
using (or the cables that came with a different PSU), the system can:

1. **Warn on incompatible cables**: "You have a Corsair RM850x (Type 4)
   but this SATA cable has a Type 3 pinout — using it will send 12V to
   your drives' 5V rail and **destroy them**."

2. **Confirm compatible cables**: "This PCIe cable is compatible with your
   EVGA G3 — same connector family."

3. **Identify unknown cables**: If the user has a box of modular cables
   with no labels, the system can match them to known pinout families
   from the device database. (Visual identification from photos is a
   future feature — cable connector colours and shapes vary by family.)

**Other pinout use cases**:

- **Ethernet crossover detection**: T568A on one end and T568B on the other
  = crossover cable. Both T568B = straight-through. Modern auto-MDIX makes
  this irrelevant for GbE, but some legacy 100M devices care.

- **Audio cable type identification**: A 3.5mm cable could be TRS (stereo)
  or TRRS (stereo + mic). Plugging TRRS into a TRS-only jack may ground
  the mic ring and cause crosstalk. The database knows which devices
  expect which.

- **MIDI DIN pin usage**: Standard 5-pin DIN MIDI uses pins 4 and 5.
  Some older synths use non-standard pin assignments. Known in the database.

- **Fan header pinout**: 3-pin (voltage control) vs 4-pin (PWM control).
  Plugging a 3-pin fan into a 4-pin header works but loses PWM — the
  board falls back to voltage control (or may not control at all,
  running the fan at full speed).

### 15.13 Compatibility and Fitment Rules

The device database contains all the data needed to evaluate whether
components are physically and electrically compatible. The compatibility
engine evaluates rules against pairs (or sets) of components.

**Compatibility check structure**:

```yaml
CompatibilityCheck:
  component_a: DeviceRef        # first component
  component_b: DeviceRef        # second component (or slot/location)
  checks: CheckResult[]         # all evaluated rules

CheckResult:
  rule: string                  # rule identifier
  category: CheckCategory       # what kind of check
  result: string                # "compatible", "incompatible", "warning", "unknown"
  severity: string              # "info", "warning", "error", "danger"
  message: string               # human-readable explanation
  recommendation: string?       # what to do about it
  data: PropertyBag?            # supporting data (measurements, limits, etc.)

CheckCategory: enum
  physical_fitment              # does it physically fit?
  electrical_compatibility      # are the interfaces electrically compatible?
  bandwidth_constraint          # does this combination create a bottleneck?
  power_adequacy                # is there enough power?
  thermal_clearance             # does cooling work with this combination?
  pinout_safety                 # are cable/connector pinouts compatible? (DANGER level)
  performance_impact            # does this combination degrade performance?
  feature_compatibility         # do features work together? (XMP + CPU, VRR + GPU, etc.)
```

**Rule categories and examples**:

**Physical fitment**:
- GPU length ≤ case max GPU clearance
- CPU cooler height ≤ case max cooler clearance
- PSU length ≤ case max PSU clearance
- RAM module height vs CPU cooler clearance at DIMM slot positions
- Radiator size vs case radiator mount dimensions
- Motherboard form factor ∈ case supported form factors
- M.2 drive length ≤ M.2 slot supported lengths (2230, 2242, 2260, 2280)
- GPU slot width (2-slot, 2.5-slot, 3-slot) vs available case expansion slots

**Electrical compatibility**:
- CPU socket matches motherboard socket
- RAM type matches motherboard DIMM type (DDR4 ≠ DDR5, physically keyed differently)
- PCIe device generation vs slot generation (backwards compatible but lower speed)
- M.2 key type (M key NVMe vs B+M key SATA — different devices, same-ish slot)
- USB-C cable capabilities vs port capabilities (USB 2.0 cable in USB 3.2 port)
- Display cable version vs desired resolution/refresh (HDMI 2.0 cable caps 4K at 60Hz)

**Bandwidth constraints**:
- PCIe device in slot with fewer electrical lanes than optimal
- Multiple devices sharing chipset DMI/PCIe uplink
- M.2 slot sharing lanes with SATA ports (NVMe install disables SATA)
- Multiple USB 3.0 devices on same hub controller (shared 5 Gbps)
- DP MST daisy-chain exceeding single link bandwidth

**Power adequacy**:
- Total component TDP vs PSU wattage (with recommended headroom)
- GPU requires more PCIe power connectors than PSU provides
- 12VHPWR sense pin configuration vs PSU capability (150W/300W/450W/600W)
- USB port power budget vs connected device requirements
- PoE budget vs connected PoE device requirements

**Pinout safety** (severity: DANGER):
- Modular PSU cable from wrong PSU family → **component destruction risk**
- ATX 4-pin EPS in 8-pin EPS socket (works but limits CPU power)
- SATA power with missing 3.3V pin (some PSUs omit it — affects some SSDs)

**Performance impact**:
- RAM not in optimal slots for dual-channel (§15 DimmTopology)
- XMP/EXPO profile speed exceeds CPU memory controller rated speed (may work OC'd, not guaranteed)
- Single-rank vs dual-rank DIMMs (dual-rank = ~5% more bandwidth but harder on memory controller)
- PCIe Gen 3 device in Gen 5 slot (wastes slot capability, but device works fine)
- VRR/FreeSync/G-Sync compatibility between GPU and monitor

**Thermal clearance**:
- Tower cooler orientation vs RAM slot clearance (tall cooler + tall RAM = conflict)
- Top-mount radiator vs tall RAM/VRM heatsink interference
- GPU length vs front-mount radiator fan thickness

**API**:

```
POST /api/v1/device-db/compatibility-check
# Body: { components: [device_id_1, device_id_2, ...], context: "pc_build" }
# Returns: all compatibility checks between all component pairs

GET /api/v1/device-db/compatible-with/{device_id}?category=motherboard
# Returns: all compatible motherboards for this CPU (or any other category)

POST /api/v1/device-db/build-validate
# Body: { build: { cpu: "...", motherboard: "...", ram: [...], gpu: "...", ... } }
# Returns: full build validation — all checks, all warnings, all recommendations.
# Includes: bandwidth graph showing every bottleneck, power budget analysis,
# physical fitment for the specified case, and performance recommendations.
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
