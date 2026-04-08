# Transports

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in
[RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

A transport moves data between two ports in the Ozma routing graph,
potentially across machine boundaries. This specification defines the
transport plugin interface, multiplexed connection model, transport
characteristics, and connection models for Bluetooth, WiFi, serial, and
constrained/exotic transports. It enumerates both the built-in transports
shipped with Ozma and the contract by which third-party exotic transports
may be added.

## Specification

### Transport plugin interface

A transport plugin represents a class of data-carrying link. Each plugin
is responsible for discovering available links, reporting capabilities,
measuring link quality, and opening/closing data streams.

Transport plugins MUST implement `discover_links`, `capabilities`,
`open`, and `close`. Transport plugins SHOULD implement `measure` and
`on_link_change`.

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
    # This MAY inject probe traffic (bandwidth test, latency ping).
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

### Multiplexed connections

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

**Priority scheduling**: Multiplexed connections MUST use strict priority
scheduling for realtime channels. When the underlying transport has limited
bandwidth (WiFi, WireGuard over internet, serial), the connection
multiplexer ensures `realtime` channels (HID) are never starved by `normal`
channels (video). A keypress is 8 bytes and MUST go out immediately; a
video frame is 50 KB and can wait one packet slot. Higher priority channels
are always serviced first. Within the same priority, round-robin.

On high-bandwidth transports (wired Gigabit LAN), priority scheduling is
irrelevant -- there's enough bandwidth for everything simultaneously. The
multiplexer detects this and MAY skip the scheduling overhead.

**Session rekeying**: Long-lived connections (a node that's been connected
for weeks) rotate encryption keys periodically without dropping the
connection. Session rekeying MUST NOT interrupt data flow. Default: rekey
every 1 GB of data or every 1 hour, whichever comes first. During
rekeying, data continues to flow -- the old keys are used until the new
keys are established, then traffic switches atomically.

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

**Subsystem advertisement**: Both sides MUST advertise supported subsystems
at connection establishment. When a connection is established, each side
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
not `hid`. This is capability negotiation at the connection level -- before
any pipeline is assembled.

**Relationship to the transport plugin interface**: Multiplexing is an
OPTIONAL capability of a transport plugin. Transports that support it
expose channels; transports that don't (e.g., a raw serial link, a VBAN
UDP stream) carry a single data type per link as before. The `open()`
method on the transport plugin returns either a simple `StreamHandle` or
a `MultiplexedConnection` with channel management methods. The router
adapts to whichever the transport provides.

### Transport characteristics

**TransportCharacteristics** -- baseline expectations before any measurement:

```yaml
TransportCharacteristics:
  expected_latency: LatencySpec           # typical latency range for this transport type
  expected_jitter: JitterSpec             # typical jitter range
  expected_loss: LossSpec                 # typical loss rate
  expected_bandwidth_bps: BandwidthSpec?  # typical bandwidth (if applicable)
  quality: InfoQuality                    # always "spec" — these are transport-class defaults
```

These are `spec` quality values -- they represent what you'd typically expect
from this class of transport under normal conditions. They are used as initial
assumptions when a link is first discovered, before any measurement. Once
`measured` data is available, it overrides these baselines.

**Expected characteristics by transport type**:

| Transport | Expected latency (p50) | Expected jitter (p95) | Expected loss | Notes |
|-----------|----------------------|----------------------|---------------|-------|
| `local-pipe` | <0.1ms | <0.01ms | 0 | Kernel IPC, essentially zero |
| `pipewire` | <1ms | <0.1ms | 0 | Same-machine, kernel scheduling |
| `usb-gadget` | <1ms | <0.5ms | 0 | USB polling interval dependent |
| `v4l2` | 1--5ms | <1ms | 0 | Frame capture timing |
| `udp-direct` (wired LAN) | <0.5ms | <0.1ms | <0.001% | Switched Ethernet, near-zero jitter |
| `udp-aead` (wired LAN) | <0.5ms | <0.1ms | <0.001% | Same + encryption overhead |
| `udp-direct` (WiFi 5/6) | 1--5ms | 1--10ms | 0.1--1% | Contention, retransmits, variable |
| `udp-aead` (WiFi 5/6) | 1--5ms | 1--10ms | 0.1--1% | Same + encryption overhead |
| `wireguard` (LAN) | 0.5--1ms | <0.2ms | <0.001% | Tunnel overhead on local network |
| `wireguard` (Internet, fibre) | 5--50ms | 1--5ms | <0.1% | ISP dependent, generally stable |
| `wireguard` (Internet, cable) | 10--60ms | 2--15ms | <0.5% | DOCSIS contention, bufferbloat |
| `wireguard` (Internet, DSL) | 15--80ms | 5--20ms | <1% | Last-mile variable |
| `wireguard` (satellite, LEO) | 20--60ms | 5--30ms | 0.5--3% | Starlink: variable, weather-dependent |
| `wireguard` (satellite, GEO) | 500--700ms | 10--50ms | 1--5% | Geostationary: high but stable latency |
| `wireguard` (cellular 4G) | 20--80ms | 10--50ms | 1--5% | Highly variable, tower handoff |
| `wireguard` (cellular 5G) | 5--30ms | 2--15ms | 0.5--2% | Better than 4G, still variable |
| `bluetooth` (Classic) | 5--20ms | 2--10ms | <1% | Adaptive frequency hopping |
| `bluetooth` (BLE) | 7--30ms | 5--20ms | <2% | Connection interval dependent |
| `serial` | <1ms | <0.5ms | 0 | Point-to-point, baudrate limited |
| `websocket` (LAN) | 1--5ms | <1ms | 0 | TCP, head-of-line blocking possible |
| `websocket` (Internet) | 10--100ms | 5--30ms | <0.5% | TCP + TLS + ISP |
| `webrtc` (LAN) | 1--5ms | <1ms | <0.1% | DTLS/UDP, low overhead |
| `webrtc` (Internet) | 10--100ms | 5--30ms | 0.5--2% | STUN/TURN dependent |
| `sunshine` | 5--15ms | 1--5ms | <0.1% | Optimised for LAN game streaming |
| `mqtt` | 5--50ms | 2--20ms | <0.5% | Broker-dependent, TCP |
| `cec` | 50--200ms | 20--100ms | <1% | Slow bus, single-wire |
| `ir` | 50--200ms | 10--50ms | 1--5% | Line-of-sight, no feedback |

These values are starting points. The router refines them with measurement.
The important insight is that the router can make reasonable initial decisions
even before probing -- it knows that a WiFi link will have 10--100x the jitter
of a wired LAN link, and a satellite hop will have 1000x the latency of a
local pipe. This informs path selection immediately on graph construction,
before any measurement traffic is injected.

**Path classification**: The router can also infer expected characteristics
from the network path type when a specific transport's characteristics depend
on the underlying network. A `wireguard` tunnel's jitter depends on whether
it's running over wired LAN, WiFi, fibre internet, or satellite. The transport
plugin reports the detected path class (from traceroute analysis, interface
type, or user configuration), and the router selects the matching baseline
row from the table above.

### Bluetooth connection model

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

**Bluetooth audio codecs** -- what the router needs to know for format
negotiation and intent matching:

| Codec | Bitrate | Sample rate | Latency | Lossy | Notes |
|-------|---------|-------------|---------|-------|-------|
| SBC | 198--345 kbps | 44.1/48 kHz | 30--50ms | Yes | Mandatory A2DP. Worst quality, always available. |
| AAC | 256 kbps | 44.1/48 kHz | 50--80ms | Yes | Good quality. Higher latency (encoder complexity). |
| aptX | 352 kbps | 44.1/48 kHz | ~40ms | Yes | Qualcomm. CD-like quality. |
| aptX HD | 576 kbps | 48 kHz/24-bit | ~40ms | Yes | Qualcomm. Hi-res. |
| aptX Adaptive | 280--420 kbps | 48/96 kHz | 50--80ms | Yes | Qualcomm. Variable bitrate, adaptive latency. |
| aptX Lossless | ~1 Mbps | 44.1 kHz/16-bit | ~40ms | No* | Qualcomm. CD lossless (*falls back to lossy if bandwidth constrained). |
| LDAC | 330/660/990 kbps | Up to 96 kHz/24-bit | 40--60ms | Yes | Sony. Best A2DP quality at 990 kbps. |
| LC3 (LE Audio) | 16--345 kbps | 8--48 kHz | 7--10ms | Yes | Bluetooth 5.2+. Low latency. Future standard. |
| LC3plus | 16--672 kbps | Up to 96 kHz | 5--10ms | Yes | Enhanced LC3. Hi-res + low latency. |
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
marked `degraded`. This feeds into re-evaluation triggers -- the router
MAY switch to a wired path if Bluetooth quality degrades.

**Multi-device**: Bluetooth can maintain multiple simultaneous connections
(Classic + BLE, or multiple BLE). Each connection is a separate link in
the graph with its own profile, codec, and quality metrics.

### WiFi connection model

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
  # Frequent roaming indicates marginal coverage — the router SHOULD prefer
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
| > -50 dBm | > 40 dB | Excellent | 80--100% of PHY rate | <2ms | Full bandwidth, all intents |
| -50 to -60 | 30--40 | Good | 60--80% | 2--5ms | Most intents OK; prefer wired for gaming/creative |
| -60 to -70 | 20--30 | Fair | 40--60% | 5--15ms | Degrade video quality; HID OK; audio MAY glitch |
| -70 to -80 | 10--20 | Poor | 20--40% | 15--50ms | HID and control only; route media over wired |
| < -80 dBm | < 10 | Unusable | Unreliable | > 50ms | Mark link as degraded; failover |

**Channel utilisation**: Even with strong signal, a congested channel
degrades performance. If `channel_utilisation_percent` > 70%, the router
treats the link as if RSSI were 10 dBm worse. This is how apartment
buildings with 30 WiFi networks on channel 6 get correctly modelled -- the
signal is strong but the medium is saturated.

**Band selection awareness**: The router knows that 2.4 GHz has longer
range but less bandwidth and more interference than 5 GHz, and that 6 GHz
(WiFi 6E/7) has the most bandwidth but shortest range. If a device supports
multiple bands, the link's expected characteristics depend on which band
is active. A device on 5 GHz channel 36 at 80 MHz width has very different
properties than the same device on 2.4 GHz channel 1 at 20 MHz.

### Serial connection model

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
that's 11,520 bytes/sec -- plenty for control commands, inadequate for media.

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
polling interval -- every byte waits up to 1ms before being delivered to the
host. At high-speed USB this drops to 125us. Native UART (direct SBC GPIO
pins) has no USB overhead -- latency is just wire propagation + one bit time.

| Interface | Added latency | Notes |
|-----------|--------------|-------|
| Native UART (SBC GPIO) | <0.1ms | Direct hardware, no stack overhead |
| USB-serial (full-speed) | 1--2ms | USB polling interval + driver |
| USB-serial (high-speed) | 0.1--0.5ms | Faster polling + driver |
| USB-serial via hub | 1--5ms | Hub adds another polling stage |
| Bluetooth SPP | 10--30ms | BT Classic serial port profile |
| WiFi serial bridge | 5--20ms | ESP32/ESP8266 WiFi-to-serial |
| Virtual serial (socat/pty) | <0.1ms | Software pipe, kernel IPC |

**Persistent identification**: Serial ports change names across reboots
(`/dev/ttyUSB0` might become `/dev/ttyUSB1`). Implementations MUST use
stable identification -- `persistent_id` uses udev `by-id` or `by-path`
symlinks, or USB serial number. A device database entry for a USB-serial
adapter includes its VID/PID and chipset, enabling automatic
re-identification.

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

RS-485 buses are modelled as a single link with multiple devices -- the bus
is a shared medium, like WiFi. Bandwidth is shared among all devices. The
router knows not to expect full throughput when multiple devices are active.

**Serial as control path**: Most serial links in Ozma carry control commands,
not media data. A TESmart matrix switch connected via USB-serial at 9600 baud
needs a few bytes per command -- the bandwidth is irrelevant, but the latency
matters for pipeline activation. The serial link's activation time is
essentially the command round-trip: send command + wait for response (if
the device confirms) or send command + assume success (if write-only).

**Serial console as data stream**: A serial console (BIOS output, bootloader
logs, kernel messages) is a data source. The serial link carries text at
whatever baud rate is configured. The node captures this via
`NodeSerialCapture` and exposes it as a data port in the graph. The text
can be displayed in the dashboard terminal view or processed by OCR triggers.

### Constrained and exotic transports

Not every transport carries high-bandwidth media. Some transports are
extremely low-bandwidth but serve important functions -- sensor data,
control commands, presence detection, or telemetry from remote locations.
The routing graph models these with the same primitives but different
characteristic profiles.

**Constrained transport characteristics**:

| Transport | Bandwidth | Range | Latency | Use in Ozma |
|-----------|-----------|-------|---------|-------------|
| LoRa | 0.3--50 kbps | 2--15 km | 100ms--5s | Remote sensor data, presence, alerts from distant buildings/farms |
| Zigbee | 250 kbps | 10--100m (mesh) | 15--30ms | IoT sensors, door contacts, motion detectors |
| Z-Wave | 100 kbps | 30--100m (mesh) | 15--30ms | IoT sensors, locks, thermostats |
| Thread/Matter | 250 kbps | 10--100m (mesh) | 10--30ms | IP-based IoT mesh (newer devices) |
| Sub-GHz (433/868/915 MHz) | 1--100 kbps | 0.5--5 km | 50ms--1s | Custom sensors, weather stations, gate controllers |
| Power line (HomePlug/G.hn) | 50--2000 Mbps | Same circuit | 5--30ms | Networking through existing wiring |
| IrDA | 9.6--16000 kbps | <1m, line of sight | <5ms | Legacy data transfer |
| NFC | 424 kbps | <10cm | <1ms | Badge/tag read for workspace profiles |
| UWB | 6.8--27.2 Mbps | 10--30m | <1ms | Precise positioning (~10cm accuracy) |

**Constrained transports as plugins**: These follow the same transport
plugin contract. A LoRa plugin discovers LoRa gateways and devices,
reports links with appropriate characteristics (50 kbps, 2s latency), and
opens/closes data streams. The router knows not to route video over LoRa
because the bandwidth constraint eliminates it -- but it happily routes
sensor data or control commands.

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
  humidity sensor -> gateway on the controller's building -> sensor device in
  the graph -> monitoring dashboard + trend alerts + automation triggers.
  No WiFi needed at the outbuilding.

- **Gate/door status**: Sub-GHz contact sensor on a driveway gate, 500m from
  the house -> received by controller with sub-GHz radio -> event triggers
  doorbell alert or security scenario.

- **Workspace presence via NFC**: Tap NFC badge at desk -> workspace profile
  activates -> scenarios, audio routing, screen layout all switch. The NFC
  read is a near-zero-latency, near-zero-bandwidth transport that carries
  identity data.

- **UWB positioning**: UWB anchors in a room provide ~10cm positioning
  accuracy -> feeds `UserZone` with `measured` quality position data ->
  spatial routing decisions based on actual position, not inferred from
  which keyboard is active.

- **Power line networking**: A node in a garage that can't be wired with
  Ethernet but is on the same electrical circuit -> HomePlug adapter gives
  50--200 Mbps, enough for KVM video. The transport characteristics table
  gives it appropriate jitter/latency expectations.

### Built-in transports

The following transports are shipped with Ozma:

| Transport ID | Description |
|-------------|-------------|
| `udp-direct` | Raw UDP, no encryption. LAN only. |
| `udp-aead` | UDP with XChaCha20-Poly1305 per-packet encryption. Default for LAN. |
| `wireguard` | WireGuard tunnel. For remote access, Connect relay. |
| `pipewire` | PipeWire link (audio, same machine). |
| `local-pipe` | Unix domain socket or pipe (same machine). |
| `usb-gadget` | USB gadget interface (HID, audio, video via configfs). |
| `v4l2` | V4L2 device interface (capture card -> userspace). |
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

Audio packets SHOULD be marked with DSCP EF (46) on transports that carry
IP traffic (`udp-direct`, `udp-aead`, `wireguard`, `vban`, `webrtc`).

### Example exotic transports

The following transports are not shipped with Ozma but illustrate the
extensibility of the transport plugin contract:

| Transport ID | Description |
|-------------|-------------|
| `hdmi-loopback` | HDMI output -> HDMI capture card on another machine. |
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
