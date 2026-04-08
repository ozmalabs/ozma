# Format System

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

Formats describe the shape of data at any point in the routing graph. Every port
MUST advertise its capabilities via a FormatSet, and the router MUST negotiate a
concrete format for each link before a pipeline is activated. Format negotiation
MUST follow three phases -- enumerate, restrict, fixate -- ensuring that data
flows are always well-typed and bandwidth requirements are known ahead of time.

## Specification

### Format Structure

A format is a media-type-specific description of data shape:

```yaml
Format:
  media_type: video | audio | hid | screen | rgb | control | data
  # one of the following, depending on media_type:
  video: VideoFormat?
  audio: AudioFormat?
  hid: HidFormat?
  screen: ScreenFormat?         # rendered frames/widgets for small screens
  rgb: RgbFormat?               # LED color data for RGB endpoints
  control: ControlFormat?       # control surface input/feedback
  data: DataFormat?
```

Every port MUST include exactly one media-type-specific sub-structure matching
its `media_type` field. Consumers MUST ignore unrecognised media types.

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
  channel_layout: string?       # "stereo", "5.1", "7.1", "7.1.4", "custom"
  channel_map: string[]?        # per-channel labels: ["FL","FR","FC","LFE","SL","SR"]
```

When `channel_layout` is `"custom"`, a `channel_map` MUST be provided. For
standard layouts (`"stereo"`, `"5.1"`, `"7.1"`, `"7.1.4"`), `channel_map` is
OPTIONAL -- if omitted, the canonical channel order for that layout MUST be
assumed.

**HidFormat**:

```yaml
HidFormat:
  device_type: string           # "keyboard", "mouse", "gamepad", "tablet", "consumer"
  report_rate_hz: uint          # how often reports are sent
  report_size_bytes: uint       # size of each report
  protocol: string              # "boot", "report", "ozma-extended"
  absolute_positioning: bool    # true for tablets/touchscreens, mouse uses absolute
```

**ScreenFormat** (rendered content for small/embedded screens):

```yaml
ScreenFormat:
  encoding: string              # "raw_rgb", "raw_rgb565", "jpeg", "png",
                                # "widget_def", "typed_data"
  resolution: Resolution?       # { w: uint, h: uint } -- native panel resolution
                                # (null for typed_data -- device handles layout)
  framerate: float?             # target refresh rate (null for event-driven updates)
  color_depth: uint?            # bits per pixel (16 for RGB565, 24 for RGB888)
  color_space: string?          # "srgb", "monochrome"
  rotation: uint?               # 0, 90, 180, 270 -- panel orientation
  dithering: bool?              # whether the sink supports dithering (for low-depth panels)
  partial_update: bool?         # can the sink accept partial frame updates?
  rendering_tier: uint          # 0=push raw frames, 1=server-rendered,
                                # 2=native render from widget defs,
                                # 3=data-driven (device has its own UI, consumes typed data)
  data_schema: DataSchema?      # for tier 3: what data fields the device accepts
```

Screen encoding `typed_data` (tier 3) is for devices that have their own
display logic and UI -- they do not receive frames or widget definitions, they
receive structured data and render it themselves. Examples: an ESP32 with a
custom firmware that shows temperature/status, a phone app showing scenario
state, a wall-mounted tablet running its own dashboard, or an e-ink display
that formats its own layout from received values.

Tier-3 devices MUST include a `data_schema` in their advertised ScreenFormat.
The controller MUST NOT send data fields that are absent from the schema.

**DataSchema** -- describes what typed data a tier-3 screen accepts:

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
publishes data, the device subscribes to what it needs. A tier-3 device MAY
advertise that it accepts `{active_scenario: string, node_count: number,
cpu_temp: number, alerts: list}` and the controller pushes updates when those
values change. The device renders however it wants -- the controller does not
know or care about the device's UI.

**RgbFormat** (LED color data for addressable LEDs, RGB zones, fixtures):

```yaml
RgbFormat:
  encoding: string              # "rgb888", "rgb565", "rgbw", "hsv", "ddp", "artnet", "e131"
  led_count: uint               # number of individually addressable LEDs
  framerate: float              # refresh rate (typically 30-60 fps)
  zones: uint?                  # number of addressable zones (if not per-LED)
  color_depth: uint             # bits per channel (8 for RGB888, 5/6/5 for RGB565)
  white_channel: bool?          # RGBW support
  gamma_corrected: bool?        # whether data is pre-gamma-corrected
```

**ControlFormat** (control surface input and feedback):

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

**DataFormat** (for arbitrary data streams -- clipboard, file transfer, etc.):

```yaml
DataFormat:
  encoding: string              # "raw", "protobuf", "json", "msgpack"
  schema: string?               # schema identifier if applicable
  max_message_size: uint?       # maximum single message size in bytes
```

### FormatSet -- Capability Advertisement

Every port MUST advertise its capabilities via a FormatSet. A FormatSet
describes everything a port can handle as a list of formats with optional
ranges:

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

This is directly inspired by PipeWire's SPA format enumeration -- ports
advertise ranges, and the router computes intersections to find compatible
formats.

### Format Negotiation

Format negotiation MUST follow three phases, adapted from PipeWire's model for
distributed operation:

**Phase 1: Enumerate**

Each port in a candidate pipeline MUST report its FormatSet. The router
collects all of them.

**Phase 2: Restrict**

The router MUST compute the intersection of FormatSets across each link. If
the intersection is empty, a converter MUST be inserted (adding a hop) or the
pipeline MUST be rejected.

The intent's constraints and preferences further restrict the intersection:
- Constraints MUST remove formats that violate hard limits
- Preferences SHOULD rank remaining formats

**Phase 3: Fixate**

The router MUST select one concrete format per link. Selection criteria (in
priority order):

1. MUST satisfy all constraints
2. SHOULD minimise conversions (prefer native format passthrough)
3. SHOULD match preferences (resolution, framerate, codec, etc.)
4. SHOULD prefer hardware-accelerated codecs if `prefer_hardware_codec` is set
5. Among equal candidates, SHOULD prefer lower bandwidth consumption

Fixation MUST be **deterministic** -- given the same graph, intent, and
measurements, the router MUST always select the same format. This makes
pipelines predictable and debuggable.

**Pre-computation**: Because the graph topology is known ahead of time (devices
are discovered, capabilities are enumerated, links are measured), format
negotiation is not a runtime handshake. The router MUST pre-compute pipelines
and their formats. When a pipeline is activated, the format is already decided.
This is fundamentally different from RTSP/SDP negotiation, which happens at
session setup time. Ozma's negotiation happens at graph-change time -- device
discovery, link metric updates, intent changes.

### Bandwidth Calculation

Bandwidth calculation MUST account for the negotiated format. Every format
implies a bandwidth requirement. The router MUST calculate this as follows:

**Uncompressed video**: `width * height * bit_depth * channels * framerate` bits/sec
- Example: 1920x1080x24x1x30 = 1,492,992,000 bps (~1.5 Gbps)

**Compressed video**: `bitrate_bps` (from codec profile/level or measured)
- Example: H.264 1080p30 high profile ~ 8,000,000 bps (8 Mbps)

**Uncompressed audio**: `sample_rate * channels * bit_depth` bits/sec
- Example: 48000x2x16 = 1,536,000 bps (1.5 Mbps)
- Example: 96000x8x24 = 18,432,000 bps (18.4 Mbps)

**Compressed audio**: `bitrate_bps`
- Example: Opus stereo ~ 128,000 bps (128 Kbps)

**HID**: `report_rate_hz * report_size_bytes * 8` bits/sec
- Example: keyboard at 1000 Hz x 8 bytes = 64,000 bps (64 Kbps)
- Example: mouse at 1000 Hz x 6 bytes = 48,000 bps (48 Kbps)

HID bandwidth is negligible compared to audio and video. HID MUST NOT be
degraded to conserve bandwidth -- it SHOULD never be the bottleneck.

**Screen (raw frames)**: `width * height * color_depth * framerate` bits/sec
- Example: Stream Deck XL (96x96 per key x 32 keys as one 480x384 frame) x 24 x 15 fps
  = 66,355,200 bps (~66 Mbps raw, ~2 Mbps JPEG)
- Example: Corsair iCUE 480x480 x 24 x 30 fps = 165,888,000 bps (~166 Mbps raw, ~5 Mbps JPEG)
- Example: e-ink 400x300 x 1bpp x 0.2 fps = 24,000 bps (negligible)

**Screen (widget definitions)**: Negligible -- JSON payloads, typically <1 KB per
update, updates only on state change. Native-rendering devices (tier 2) receive
definitions, not frames.

**RGB**: `led_count * color_depth * framerate` bits/sec
- Example: 300-LED WLED strip x 24 x 30 fps = 216,000 bps (~216 Kbps)
- Example: 104-key RGB keyboard x 24 x 30 fps = 74,880 bps (~75 Kbps)
- Example: Full room (1500 LEDs) x 24 x 60 fps = 2,160,000 bps (~2.2 Mbps)
- With protocol overhead (DDP/E1.31/ArtNet): typically 1.5-2x raw

**Control surface input**: `report_rate * report_size * 8` bits/sec
- Example: MIDI at 31.25 kbaud = 31,250 bps (protocol maximum)
- Example: Stream Deck buttons at 100 Hz x 2 bytes = 1,600 bps
- Example: Gamepad at 250 Hz x 12 bytes = 24,000 bps

**Control surface feedback**: Varies widely by device
- Example: Stream Deck key images (15 keys x JPEG ~ 3 KB each) on state change
  = bursty, ~45 KB per update, rare
- Example: X-Touch scribble strips (8 x 7 chars x 2 lines) = <100 bytes per update
- Example: Motor fader position (8 faders x 2 bytes x 50 Hz) = 6,400 bps

Screen, RGB, and control surface bandwidth is small compared to video and
audio. However, the rendering cost on the controller MAY be significant --
implementors SHOULD account for device capacity and endpoint-specific resource
usage when planning deployments.
