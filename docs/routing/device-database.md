# Physical Device Database

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119].

[RFC 2119]: https://datatracker.ietf.org/doc/html/rfc2119

## Abstract

The Physical Device Database is a universal, open, community-contributed
catalog of physical device definitions that provides the routing graph with
detailed knowledge about the real-world properties of hardware — dimensions,
internal topology, port capabilities, performance characteristics, frequency
response curves, 3D models, and compatibility data — enabling smarter routing
decisions, safety warnings, and user-facing recommendations across the entire
Ozma mesh.

## Specification

The device database is a universal, open, community-contributed catalog of
physical device definitions. It provides the routing graph with detailed
knowledge about the real-world properties of hardware — dimensions, internal
topology, port capabilities, performance characteristics, frequency response
curves, 3D models, and anything else that helps Ozma be a better router.

### 1. Design principles

**Minimal required fields**: A device entry MUST have an `id`, a `type`, and
a `name`. Everything else is OPTIONAL. A bare-minimum entry gets a device into
the graph with `assumed` quality properties. As the community contributes more
detail, the entry gets richer and routing decisions get better.

**Maximum flexibility**: The schema is extensible by design. Any field not in
the core schema can be added as a typed extension. If one user discovers that
a specific Thunderbolt dock uses USB 2.0 internally, they add an
`internal_topology` block. That block becomes available to everyone with that
dock.

**Open and community-driven**: The database is public, version-controlled, and
accepts contributions. Anyone can submit a new device entry, correct an existing
one, or add detail. Community contributions SHOULD be validated against the
JSON schema before acceptance. Contributions are reviewed and quality-tagged.

**Distributed via Connect**: The canonical database lives on Connect. Controllers
download the portions they need. Large controllers with storage cache
everything and distribute via mesh to nodes. Small nodes download only entries
matching their detected hardware.

### 2. Entry schema

Every device in the database MUST follow this schema. Only `id`, `type`, and `name`
are REQUIRED. Everything else is OPTIONAL and typed.

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
  audio: AudioSpec?             # microphones, speakers, audio interfaces
  headphone: HeadphoneSpec?     # headphones, headsets, earbuds (wired + wireless)
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

### 3. Inheritance

Entries can inherit from a parent using `inherits`. The system MUST resolve
the parent entry before applying child field overrides. This avoids duplicating
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

### 4. Category-specific blocks

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

**HeadphoneSpec** (headphones, headsets, earbuds — compound audio devices):

A headphone/headset is a compound device: audio sinks (speakers), optionally
an audio source (microphone), controls (play/pause/ANC/volume), and battery.
The connection type determines transport, codec, and latency.

```yaml
HeadphoneSpec:
  form_factor: string           # "over_ear", "on_ear", "in_ear", "earbuds",
                                # "bone_conduction", "open_ear"
  type: string                  # "headphone", "headset", "gaming_headset",
                                # "aviation_headset", "monitoring_headphone"

  # --- Connection ---
  connection: HeadphoneConnection[]  # MAY support multiple (wired + BT + USB dongle)

  # --- Audio ---
  driver_size_mm: float?
  driver_type: string?          # "dynamic", "planar", "balanced_armature",
                                # "electrostatic", "bone_conduction", "hybrid"
  impedance_ohm: float?         # 16Ω earbuds, 32–80Ω typical, 300Ω studio
  sensitivity_db: float?
  frequency_response: FrequencyResponse?
  open_back: bool?              # open-back (wider soundstage, leaks sound)
  noise_isolation_db: float?    # passive isolation (closed-back)

  # --- Microphone ---
  microphone: HeadphoneMicSpec?

  # --- Active features ---
  anc: AncSpec?                 # active noise cancellation
  transparency_mode: bool?      # ambient awareness / pass-through
  sidetone: bool?               # hear own voice in earpiece
  spatial_audio: bool?          # head-tracked spatial (Apple, Sony)
  spatial_audio_type: string?   # "head_tracked", "fixed", "none"

  # --- Controls ---
  controls: HeadphoneControls?

  # --- Battery ---
  battery: BatterySpec?
  battery_life_hours: float?    # rated (ANC on)
  battery_life_anc_off_hours: float?
  charging: string?             # "usb_c", "case_wireless", "case_usb_c", etc.
  quick_charge: string?         # "5min_60min"

  # --- Physical ---
  weight_g: float?
  replaceable_pads: bool?
  replaceable_cable: bool?
  foldable: bool?

HeadphoneConnection:
  type: string                  # "bluetooth", "usb_c_wired", "3.5mm_wired",
                                # "6.35mm_wired", "usb_dongle_2_4ghz", "dect"
  bluetooth: BluetoothSpec?     # if BT: version, supported codecs, profiles
  multi_point: bool?            # connect to 2+ sources simultaneously
  multi_point_max: uint?        # max simultaneous connections
  dongle_connection: string?    # "usb_a", "usb_c"
  simultaneous_wired_wireless: bool?
  # Multi-point is a routing concern: headset connected to phone (BT) AND
  # PC (USB dongle) — Ozma's mix bus handles which source plays based on
  # priority. The headset's own multi-point behaviour means it makes routing
  # decisions the controller SHOULD be aware of.

HeadphoneMicSpec:
  mic_type: string              # "boom", "inline", "built_in", "detachable_boom",
                                # "retractable_boom", "beamforming_array"
  noise_cancelling_mic: bool?   # ANC on the mic (for calls in noisy environments)
  mic_mute: string?             # "button", "boom_flip", "app", "none"
  frequency_response: FrequencyResponse?
  pickup_pattern: string?       # "cardioid", "omnidirectional", "beamforming"
  sidetone_adjustable: bool?

AncSpec:
  type: string                  # "feedforward", "feedback", "hybrid"
  levels: uint?                 # number of ANC levels (1 = on/off, 3+ = adjustable)
  adaptive: bool?               # auto-adjusts to environment
  wind_noise_reduction: bool?
  rated_reduction_db: float?
  # ANC affects the routing graph: when ANC is on, the effective noise floor
  # at the user's ears drops by 20–30 dB. The thermal management system
  # (§2.14) SHOULD relax fan noise limits for noise-sensitive scenarios —
  # the user can't hear the fans. Intent binding: "ANC headphones connected
  # → allow server fans up to 45 dBA instead of 30 dBA."

HeadphoneControls:
  type: string                  # "physical_buttons", "touch_surface", "stem_squeeze"
  volume: bool?
  play_pause: bool?
  skip_track: bool?
  voice_assistant: bool?
  anc_toggle: bool?
  customisable: bool?
  # Controls are a control surface in the graph (§2.8). Ozma intent bindings
  # MAY map headphone gestures to actions: "triple-tap right earbud = switch
  # scenario."
```

**Codec affects routing**: A Bluetooth headset on SBC (~200 kbps, lossy)
sounds significantly worse than on LDAC (990 kbps) or LC3 (LE Audio). The
`fidelity_audio` intent MUST reject SBC. The router MUST check the negotiated
Bluetooth codec against the intent's format constraints before routing audio
to the headset.

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

**Building management device types** — locks, HVAC, lighting, and occupancy
sensors are devices in the graph with control paths, state, events, and
physical locations. They use the same primitives as everything else — the
building management system is not a separate product, it's a natural
extension of the routing graph into the physical environment.

```yaml
LockSpec:
  lock_type: string             # "smart_lock", "badge_reader", "keypad",
                                # "intercom", "electric_strike", "mag_lock",
                                # "turnstile", "garage_door"
  authentication: string[]      # ["badge_rfid", "badge_nfc", "pin", "fingerprint",
                                # "face", "ble_phone", "key", "remote", "buzzer"]
  credential_formats: string[]? # ["hid_iclass", "hid_prox", "mifare_classic",
                                # "mifare_desfire", "em4100", "nfc_ndef"]
  state: LockState
  control_interface: string?    # "zwave", "zigbee", "ble", "wifi", "wiegand",
                                # "osdp", "ip", "dry_contact"
  camera: bool?                 # integrated camera (video doorbell, intercom)
  two_way_audio: bool?          # intercom capability
  auto_lock: bool?              # re-locks after timeout
  door_sensor: bool?            # detects door open/closed (not just locked/unlocked)
  tamper_detection: bool?       # detects physical tampering
  battery: BatterySpec?         # for battery-powered locks
  fire_release: bool?           # unlocks on fire alarm (safety requirement)
  fail_mode: string?            # "fail_secure" (stays locked on power loss) or
                                # "fail_safe" (unlocks on power loss — for fire exits)

LockState:
  locked: bool
  door_open: bool?              # door position (if sensor present)
  last_access: LockAccessEvent?
  battery_percent: float?

LockAccessEvent:
  timestamp: timestamp
  action: string                # "unlock", "lock", "denied", "forced_entry", "held_open"
  method: string?               # "badge", "pin", "fingerprint", "ble", "remote", "key", "exit_button"
  credential_id: string?        # which badge/code was used (not the person — privacy)
  # Access events feed into the state change journal (§11.6) and can
  # trigger intent bindings (§8.7): "office door unlocked → lights on,
  # HVAC to occupied mode, WiFi APs at full power"

HvacSpec:
  hvac_type: string             # "thermostat", "crac", "crah", "split_system",
                                # "fan_coil", "vrf", "ptac", "baseboard", "radiant"
  heating: bool?
  cooling: bool?
  fan: bool?
  humidity_control: bool?
  temperature_range_c: { min: float, max: float }?
  control_interface: string?    # "zwave", "zigbee", "wifi", "modbus", "bacnet",
                                # "ip", "ir", "dry_contact"
  zones: uint?                  # number of independently controlled zones
  schedule: bool?               # supports scheduling
  occupancy_aware: bool?        # has or accepts occupancy input
  state: HvacState

HvacState:
  mode: string                  # "heating", "cooling", "auto", "fan_only", "off"
  current_temp_c: float?
  target_temp_c: float?
  humidity_percent: float?
  fan_speed: string?            # "auto", "low", "medium", "high"
  running: bool?                # compressor/heater currently active

LightingSpec:
  lighting_type: string         # "smart_switch", "smart_dimmer", "smart_bulb",
                                # "scene_controller", "occupancy_switch",
                                # "daylight_harvesting", "emergency_lighting"
  dimmable: bool?
  color_temp: bool?             # adjustable color temperature (warm/cool white)
  color: bool?                  # full RGB color (overlaps with rgb device type — this is room lighting, not accent/indicator)
  control_interface: string?    # "zwave", "zigbee", "wifi", "dali", "dmx",
                                # "0_10v", "phase_cut", "ip"
  wattage: float?
  lumens: float?
  circuits: uint?               # number of independently controllable circuits
  emergency: bool?              # emergency lighting (battery backup, legally required)
  state: LightingState

LightingState:
  on: bool
  brightness_percent: float?    # 0–100
  color_temp_k: uint?           # color temperature in Kelvin
  scene: string?                # active scene name

OccupancySpec:
  sensor_type: string           # "pir", "ultrasonic", "dual_tech", "mmwave_radar",
                                # "camera_ai", "desk_sensor", "thermal_array",
                                # "people_counter", "ble_beacon", "wifi_probe"
  detection_range_m: float?     # maximum detection range
  detection_angle_deg: float?   # field of detection
  people_counting: bool?        # can count people, not just detect presence
  max_count: uint?              # maximum people it can count simultaneously
  direction_detection: bool?    # can detect entry vs exit direction
  desk_level: bool?             # per-desk occupancy (under-desk sensor or booking system)
  control_interface: string?    # "zigbee", "ble", "wifi", "dry_contact", "ip"
  state: OccupancyState

OccupancyState:
  occupied: bool
  count: uint?                  # number of people detected
  last_motion: timestamp?       # when motion was last detected
  confidence: float?            # 0.0–1.0 detection confidence
```

**Building management through intent bindings (§8.7)**:

These devices don't need custom building management logic — the existing
intent binding system handles it:

```yaml
# Office opens — first person arrives
- conditions:
    - { source: lock, field: state.action, op: eq, value: "unlock" }
    - { source: occupancy, field: state.occupied, op: eq, value: false }
      # was unoccupied, now someone unlocked the door
  actions:
    - { type: hvac.set_mode, target: "office_thermostat", mode: "auto", temp: 22 }
    - { type: lighting.on, target: "office_lights", brightness: 80 }
    - { type: ap.enable, target: "office_ap" }

# Office empties — last person leaves
- conditions:
    - { source: occupancy, field: state.count, op: eq, value: 0 }
    - { source: occupancy, field: state.last_motion, op: age_gt, value: 600 }
      # no motion for 10 minutes
  actions:
    - { type: hvac.set_mode, target: "office_thermostat", mode: "off" }
    - { type: lighting.off, target: "office_lights" }
    - { type: ap.disable, target: "office_ap" }  # disable WiFi to save power
    - { type: lock.lock, target: "office_door" }

# After hours — building unoccupied
- conditions:
    - { source: time, field: hour, op: gt, value: 22 }
    - { source: occupancy, field: state.count, op: eq, value: 0 }
  actions:
    - { type: hvac.setback, temp: 16 }            # heating setback
    - { type: lighting.off, target: "all" }
    - { type: ap.reduce_power, target: "all" }    # reduce WiFi to minimum
    - { type: security.arm, target: "alarm_panel" }

# Meeting room booked — pre-condition
- conditions:
    - { source: calendar, field: event_active, op: eq, value: true }
    - { source: calendar, field: event_room, op: eq, value: "meeting_room_1" }
  actions:
    - { type: hvac.set_mode, target: "meeting_room_1_hvac", mode: "cooling", temp: 21 }
    - { type: lighting.scene, target: "meeting_room_1_lights", scene: "presentation" }
    - { type: display.power_on, target: "meeting_room_1_screen" }
    # Pre-cool and light the room before the meeting starts
```

**What Ozma adds beyond Home Assistant**: HA can do occupancy-driven
automation. The difference is that Ozma's graph also knows the IT
infrastructure — when the office empties, Ozma doesn't just turn off
lights, it also: reduces AP transmit power (saves energy, reduces RF
exposure), cools warm pipelines (saves server power), adjusts thermal
profiles on servers (quiet mode when nobody's there to hear fans),
pauses non-essential monitoring refresh cycles (saves CPU and I/O on
constrained devices), and arms the security system. The building
automation and IT management share one event bus, one intent system,
one graph.

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

### 5. 3D models

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

3D models MAY be provided for visual device recognition and 3D desk scene
rendering but are never required. The parametric shape templates
(§ device-db.md) provide a fallback rendering for any device without a 3D
model. Models are stored on Connect and downloaded on demand.

### 6. Distribution via Connect

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

### 7. Custom and private entries

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

### 8. Matching and auto-detection

When a new device appears in the graph (USB hotplug, mDNS discovery, Bluetooth
pairing), the controller MUST attempt to match it to a database entry in the
following order:

1. **USB VID/PID match**: USB VID/PID matching MUST be attempted before fuzzy
   matching. Exact match on `usb[].vid` and `usb[].pid`.
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

### 9. Quality and provenance

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

### 10. Relationship to device-db.md

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

### 11. API

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

### 12. Pinouts

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

**Safety checking**: Modular PSU cable pinout mismatches MUST trigger
DANGER-level warnings. If the user tells the system which PSU they have
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

### 13. Compatibility and Fitment Rules

The device database contains all the data needed to evaluate whether
components are physically and electrically compatible. Compatibility checks
MUST evaluate physical fitment, electrical compatibility, bandwidth
constraints, power adequacy, pinout safety, performance impact, and thermal
clearance. The compatibility engine evaluates rules against pairs (or sets)
of components.

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

