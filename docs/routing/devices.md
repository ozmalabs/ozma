# Device Types

Status: Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

## Abstract

This document specifies the additional device classes in the Ozma routing graph
beyond the core KVM and endpoint device types. Each device type MUST declare its
ports and media types. Devices that are compound, have unusual port
configurations, or integrate with external systems are covered in detail. This
includes cameras, phones, actuators, sensors, audio processors, VM hosts,
displays with DDC/CI, managed services, notification and metrics sinks, media
receivers and sources, media sessions, network switches and routers, macro
sources, and clipboard devices.

## Specification

### Cameras

A camera is a video source device. Different camera types have different
discovery mechanisms, transport types, and control capabilities. Every camera
device MUST declare its ports and media types.

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

Doorbell cameras are compound devices: video source + audio source + audio sink
(two-way) + event source (button press, motion) + control sink (unlock
command). The routing graph MUST express all of these as separate ports.

Frigate-managed cameras are discovered via the Frigate API. Their video ports
MAY source from Frigate's re-stream (RTSP) rather than directly from the
camera, adding a hop but gaining Frigate's detection events.

### Phones

A phone connected via USB or KDE Connect is a compound device with multiple
independent ports. Each port MUST declare its direction and media type.

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

Not all ports are active simultaneously. USB audio and screen mirror MAY
conflict for USB bandwidth. The router MUST respect the device's USB bus
capacity when deciding which ports to activate.

### Actuators and Motion Devices

Actuators are controllable physical devices -- monitor stands, sit/stand desks,
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

Actuators MUST declare at least one axis. Each axis MUST declare its `min` and
`max` range. Named presets are OPTIONAL.

Actuators affect the routing graph indirectly: when a desk reaches the
"standing" preset, a trigger MAY activate a different scenario which assembles
different pipelines. Actuator position also feeds the world layout (spatial
RGB) -- when a desk moves, all devices on it move in the 3D model.

### Sensors

Sensors are source devices that produce data readings. They MUST have data
source ports. Every sensor MUST declare its `data_schema` and `sample_rate_hz`.

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
  data_schema: DataSchema       # what fields this sensor produces
  sample_rate_hz: float         # how often it reads
  alert_thresholds: Threshold[]? # configured alert levels
```

Sensors feed the trigger engine, SIEM, compliance evidence, and screen widgets.
They do not participate in media pipelines (no video/audio/HID), but they MUST
be modelled as graph devices with data ports, resource costs, and version
information.

### Audio Processors

Audio processors are inline processing devices -- they sit in an audio pipeline
between source and sink, transforming the audio. They MUST have both audio sink
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

The router MUST model audio processors as hops in the pipeline. Each processor
MUST declare its `latency_ms` and `resource_cost`. A room correction filter
adds ~5ms latency and ~3% CPU. The intent's latency budget MUST accommodate
this. If a `fidelity_audio` intent has a 10ms budget and the pipeline already
uses 6ms for transport, the room correction filter (5ms) would bust the
budget -- the router MUST either skip it or the user MUST relax the constraint.

### VM Hosts and Virtual Targets

A VM host (QEMU/KVM, Proxmox, libvirt) is a device that contains virtual
machine targets. Each VM MUST be modelled as a target device with the
following ports:

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
latency. The router SHOULD prefer it over VNC/screendump when available.

### Displays with DDC/CI

Full-size displays (monitors, projectors) have control capabilities beyond
just being a video sink. Each display MUST declare its DDC/CI capabilities.

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
    brightness: bool            # adjustable brightness (0-100)
    contrast: bool              # adjustable contrast
    input_select: bool          # can switch HDMI/DP inputs
    power: bool                 # can power on/off/standby
    volume: bool                # built-in speaker volume
```

DDC/CI input switching makes a monitor behave like a simple switch -- the
router MAY command a monitor to switch its input as part of pipeline
activation. This MUST be modelled as a controllable device with `confirmed` or
`write_only` feedback depending on the monitor's DDC/CI implementation.

### Managed Services

Ecosystem services (Frigate, Jellyfin, Immich, Home Assistant, Vaultwarden,
Audiobookshelf) are devices in the graph with API ports. Each service MUST
declare its ports and health state.

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

Services do not participate in real-time media pipelines (they are not KVM
components), but they MUST be part of the graph because:

- They consume device resources -- a Frigate instance uses GPU and CPU
- They are versioned and updatable through the mesh
- They produce events that feed triggers and SIEM
- Their health affects system health reporting
- The controller MAY manage their lifecycle (start, stop, update, backup)

### Notification and Metrics Sinks

Output-only devices that consume events, alerts, or metrics. Each sink MUST
declare its sink type and connection information.

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

These are data sinks. They MUST appear in the graph so the router can track
their health, resource usage, and connectivity -- but they MUST NOT participate
in media pipeline routing.

### Media Receivers and Sources

A media receiver is a software endpoint that receives audio (and sometimes
video) from an external streaming service and produces it locally as a
PipeWire source. A media source is a controllable content library that
can be directed to play through specific outputs. Each MUST declare its ports
and media types.

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
  volume: float?                # 0.0-1.0 (service-level volume, independent of Ozma volume)
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

#### Why media receivers are in the routing graph

A Spotify Connect receiver is not just a service -- it is an audio source in
the routing graph with a PipeWire port. The router MUST know about it because:

1. **It produces audio that enters the mix bus.** When Spotify plays, its
   audio competes for the desk speakers with KVM node audio. The mix bus
   handles summing, but the router MUST know the source exists.

2. **It has format properties.** Spotify outputs Ogg Vorbis 320kbps (lossy).
   Tidal can output FLAC (lossless) or MQA. The `fidelity_audio` intent
   SHOULD reject a Spotify source but accept Tidal FLAC -- this is format
   negotiation.

3. **It can be routed to multiple outputs.** Spotify playing on the
   controller can be sent to desk speakers + AirPlay living room +
   VBAN to kitchen -- this is fan-out from one source to multiple sinks
   via audio output targets.

4. **Metadata feeds screen endpoints.** Track info and album art from
   the `metadata` port to Stream Deck key images, OLED status display,
   dashboard now-playing widget. This is a `data` pipeline from the
   media receiver to screen sinks.

5. **Intent bindings react to playback state.** "When Spotify starts
   playing, switch to music intent (lower KVM audio, enable room
   correction, set RGB to ambient mode)" -- the `PlaybackState` is a
   condition source for intent bindings.

#### Casting model

Some services can direct playback to a specific device -- Spotify Connect,
AirPlay, Chromecast. This is a control path operation: the controller tells
the service "play through this receiver". The audio then appears at that
receiver's PipeWire port. The `can_cast_to` field on MediaSourceDevice
indicates which receivers a source MAY target.

This is distinct from Ozma's own audio routing. Spotify casting routes
within Spotify's infrastructure; Ozma routing happens after the audio
reaches PipeWire. Both MAY coexist -- Spotify casts to the controller's
Spotify Connect receiver, then Ozma routes the PipeWire output to
multiple speakers via its own transport plugins.

### Network Switches and Routers

A managed network switch is a compound device with multiple Ethernet ports
connected by a switching fabric. Network switches MUST report per-port status
including link speed and PoE state.

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

Network switches and routers MUST be modelled in the routing protocol because:

- Their **port speeds and PoE budgets** constrain what devices can connect
  and at what bandwidth
- Their **backplane capacity** determines whether the switch is a bottleneck
  (a cheap gigabit switch with a 2 Gbps backplane is blocking)
- Their **VLAN configuration** affects which devices can talk to each other
  (IoT VLAN isolation)
- Their **PoE power budget** is a power model concern -- a PoE switch has a
  total power budget shared across all PoE ports
- The WAN interface has fundamentally different characteristics (latency,
  jitter, bandwidth) from LAN interfaces -- the transport characteristics
  depend on knowing whether traffic crosses the WAN

### Media Sessions on Target Machines

A target machine MAY have multiple media players running simultaneously --
Spotify playing music, YouTube paused in a browser tab, a game with its own
audio. The OS mixes all of these into a single system audio output, which
the node captures as one audio stream. But the desktop agent inside the OS
can observe each media session individually via platform APIs.

Media sessions on targets SHOULD report playback state. Media sessions on a
target are modelled as child devices of the target, reported by the desktop
agent:

```yaml
MediaSessionDevice:
  type: media_receiver          # same type as controller-side receivers
  session_source: string        # "agent" -- discovered by desktop agent, not by controller
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

#### Worked example: desktop with Spotify playing and YouTube paused

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

#### Audio separability

The `audio_separable` field indicates whether this session's audio can be
captured independently from the system mix:

| Platform | Per-app audio capture | How |
|----------|----------------------|-----|
| Linux (PipeWire) | Yes | Each app is a PipeWire stream node; agent can capture individually |
| Linux (PulseAudio) | Yes | `pactl` per-source-output capture |
| Windows 10+ | Yes | WASAPI process loopback (`AUDCLNT_PROCESS_LOOPBACK_MODE`) |
| macOS | Partial | Requires virtual audio driver (BlackHole/Loopback); per-app not native |

When audio is separable, the agent SHOULD capture individual streams and send
them to the controller as separate VBAN/Opus channels. The controller then
has per-source routing control -- Spotify audio to the desk speakers at full
quality, YouTube audio muted, game audio ducked. This is a per-application
mix bus on the target machine, bridged to the controller via the agent.

When audio is NOT separable (no agent, or platform does not support it), the
controller receives mixed system audio as a single stream. Media session
metadata SHOULD still be available (the agent reports playback state even if
it cannot separate audio), so intent bindings and screen metadata still
work -- you just cannot route individual apps.

#### How this affects the routing graph

1. **Without agent (node-only capture)**: One audio source port on the target.
   One `system_audio` stream. No per-app control. This is the basic KVM path.

2. **With agent, non-separable**: Same one audio source port, but media
   session metadata SHOULD be available as data ports. Intent bindings MAY
   react to "Spotify is playing" even though audio cannot be separated.

3. **With agent, separable audio**: Multiple audio source ports on the target,
   one per separable session. Each enters the routing graph independently.
   The controller's mix bus handles per-source volume/mute/routing. Full
   control.

#### Intent binding examples

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

### Macro and Synthetic Input Sources

The macro system and automation engine produce synthetic HID input. They MUST
be modelled as virtual source devices. Each MUST declare its ports and media
types.

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
a virtual source port. The router MUST deliver them through the same pipeline
as physical keyboard input -- the target device MUST NOT be able to
distinguish macro input from physical input.

### Clipboard

Cross-machine clipboard is a bidirectional data stream. The clipboard device
MUST declare its data schema.

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
