# Endpoint Devices

Status: Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

## Abstract

This document specifies how endpoint devices -- displays, RGB lighting, and
control surfaces -- participate in the Ozma routing graph. Endpoint devices
have ports, capabilities, resource requirements, and connection constraints
like any other device. The router MUST account for them when resolving intents,
selecting display methods, assembling pipelines, and managing device pressure.

## Specification

### Displays

A display is a visual endpoint, from a full-size monitor or projector to a
small embedded panel. A display is not necessarily a pixel sink. It MAY consume
pixel frames, renderable UI descriptions, structured data, or semantic entities
and render them using its own display method. Routing structured content to a
capable display MUST NOT require Ozma to first reconstruct that content as a
picture.

The user or application supplies desired content or an intent. The graph
matches that content to display capabilities and an available display method.
The method may run on the display, on a nearby node, or as a remote graph
service. The selected method is part of the resolution, not a fixed assumption
in the user request. A method MAY apply a style or theme, map entity attributes
to widgets, and choose layout based on the display's size, color, touch, and
character-set capabilities. Where possible, semantic calculations SHOULD be
provided as data attributes rather than embedded in presentation code.

Every display endpoint MUST advertise the representations and display methods
it accepts. A method declaration identifies accepted content types or schemas,
its rendering location, output formats, interaction support, and configuration
needs. The `DisplayMethod` contract is defined in the [format system](formats.md).
Methods and content schemas use open, namespaced identifiers so plugins and
devices can add them without extending a closed display-type enum.

For a display that renders locally, the route carries structured content to
the display method and the display emits pixels locally. For a display that
cannot render the content, the graph MAY select a compatible renderer service
and route its output to the display. Pixel reconstruction is therefore one
available method, not the universal display contract.

#### Display representation tiers

The rendering tier indicates the boundary at which content becomes pixels.
Displays MAY support multiple tiers and methods; the endpoint advertises all
supported tiers, while each `ScreenFormat` describes one representation.

| Tier | Representation delivered | Rendering location | Typical use |
|------|---------------------------|--------------------|-------------|
| 0 -- Pixel sink | Raw or compressed frames | Upstream renderer | Legacy panels and remote display streams |
| 1 -- Rendered UI | Composed widgets or UI description | Upstream renderer or display runtime | Browser widgets and configurable dashboards |
| 2 -- Native UI | Widget definitions and layout hints | Display runtime | Smart displays with a native widget framework |
| 3 -- Data-driven | Typed data or semantic entities | Display method on device or graph service | E-ink status panel, phone UI, device-specific display |

The display method MUST declare what it needs; implementations MUST NOT infer
that a tier number alone guarantees support for a particular widget, schema, or
interaction.

### Screen endpoint profile

`ScreenEndpoint` is the visual endpoint profile used for small and embedded
displays. Full-size monitors and projectors use the same content/method model
with their own resolution, color, refresh, and interaction capabilities.

Every screen endpoint MUST declare its supported rendering tiers. The tiers
and advertised methods determine where rendering happens and what formats its
sink port accepts.

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
  rendering_tiers: uint[]       # supported tiers from the table above
  display_methods: DisplayMethod[] # methods and content forms accepted
  connection: ConnectionInfo    # how this screen connects
```

The routing graph MUST model each selected display method honestly:

- **Pixel rendering**: If content requires upstream rendering, the graph MUST
  include a renderer participant with a source port and account for its
  resources. The renderer may be on the controller, a node, or another
  reachable service.

- **Structured content**: If the display method accepts typed data or semantic
  entities, the route MUST preserve that representation to the method. The
  device or selected renderer owns presentation. Resource costs are charged to
  the participant that performs the rendering.

- **Method negotiation**: The resolver MUST match content type/schema and
  required interactions against advertised methods. If no method can consume
  the content, it MAY find a renderer/converter path or report the requirement
  as unmet; it MUST NOT silently pretend that a pixel-only path preserves
  semantic structure.

#### Compound screen devices

A Stream Deck is both a control surface (buttons) and a screen (LCD per key or
full LCD panel). It MUST be modelled as a compound device with:

- Control source ports (button presses, touch events)
- Screen sink ports (key images, full-screen content)
- Possibly separate ports per key region or a single port for the whole panel

#### Connection types

```yaml
ConnectionInfo:
  transport: string             # "usb", "serial", "spi", "i2c", "wifi", "bluetooth", "network"
  bus_bandwidth_bps: uint64?    # connection bandwidth limit
  shared_bus: bool              # is this connection shared with other devices?
  latency_ms: float?            # connection latency
```

#### Screen capacity

| Screen type | Typical resolution | Max fps | Connection | Bottleneck |
|------------|-------------------|---------|------------|------------|
| Stream Deck MK.2 (15 key) | 72x72 per key | 15 | USB HID | USB HID report size |
| Stream Deck XL (32 key) | 96x96 per key | 15 | USB HID | USB HID report size |
| Stream Deck + (full LCD) | 800x100 touch strip | 30 | USB | USB bandwidth |
| Corsair iCUE screen | 480x480 | 30 | USB | USB bandwidth |
| SSD1306 OLED | 128x64 mono | 60 | I2C/SPI | Bus speed (I2C: 400 kHz) |
| Waveshare e-ink | 400x300 | 0.2 | SPI | Panel refresh (~5s) |
| LED matrix (HUB75) | 64x32 | 60 | GPIO/HUB75 | GPIO timing |
| Browser widget | Arbitrary | 60 | WebSocket | Network + browser render |
| ESP32 + TFT | 320x240 | 30 | WiFi/BLE | Wireless bandwidth |

### RGB Endpoints

An RGB endpoint is any device that accepts color data for visual output via
LEDs. RGB endpoints are data sinks.

Every RGB endpoint MUST report its LED count and maximum power draw. If the
power draw is not known, the endpoint MUST estimate it based on LED count and
LED type. The `led_count` and `power_draw_w` fields are REQUIRED.

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

#### RgbTopology

```yaml
RgbTopology:
  type: string                  # "strip", "matrix", "zones", "per_key", "ring", "custom"
  dimensions: Dimensions?       # for matrix: { w, h }. For strip: { length_mm }
  zone_count: uint?             # for zone-based (e.g., motherboard RGB headers)
  zone_names: string[]?         # human-readable zone labels
  spatial_layout: SpatialLed[]? # per-LED positions in device-local coordinates (mm)
```

#### Resource costs of RGB

The bandwidth is small, but the rendering cost can be significant when running
spatial effects across many devices:

| Scenario | LED count | Controller CPU | Notes |
|----------|----------|----------------|-------|
| Single keyboard per-key | 104 | <1% | Simple solid/gradient |
| Single WLED strip | 300 | <1% | Direct DDP output |
| Full room spatial effect | 1500+ | 3-8% | World-space effect function evaluated per LED per frame |
| Reactive key effects | 104 | 1-3% | Per-keypress effect computation |

The RGB compositor (layered rendering engine) MUST be modelled as a processing
device in the graph with resource costs on the controller.

### Control Surfaces

A control surface is any device that sends control input to the controller
and optionally receives visual/haptic feedback. Control surfaces have both
source ports (input) and optionally sink ports (feedback). Control surfaces
MAY be bidirectional.

```yaml
ControlSurfaceEndpoint:
  inputs: ControlInputSet       # buttons, faders, encoders, axes
  outputs: ControlOutputSet?    # LEDs, displays, motors
  connection: ConnectionInfo
  protocol: string              # native protocol ("midi", "osc", "hid", "serial", "streamdeck_hid")
  bidirectional: bool           # does the device accept feedback?
  pages: uint?                  # number of switchable pages/layers
  profiles: bool?               # does the device support on-device profiles?
```

A control surface that accepts feedback (LEDs, motor faders, display segments)
MUST set `bidirectional: true` and MUST declare its output capabilities in the
`outputs` field. A control surface that only produces input (no feedback path)
SHOULD set `bidirectional: false` and MAY omit the `outputs` field.

#### Control surface as compound device

Many control surfaces are compound devices. Each independent function MUST be
modelled as a separate port in the routing graph:

- **Stream Deck**: buttons (control source) + LCD keys (screen sinks) + touch
  strip (control source) + full-screen LCD (screen sink). 4+ ports.
- **X-Touch Mini**: encoders + buttons (control source) + LED rings + button
  LEDs (control sink via MIDI). 2 ports.
- **Gamepad**: axes + buttons + triggers (control source) + rumble motors
  (control sink). 2 ports.
- **ShuttlePRO v2**: jog wheel + shuttle ring + buttons (control source).
  1 port (no feedback).

The routing graph MUST model each independently. The controller MUST be able to
send screen data to a Stream Deck's LCD while simultaneously receiving button
presses -- these are separate pipelines with separate resource costs.

#### Control surface resource costs

| Device | Controller CPU | Memory | Notes |
|--------|---------------|--------|-------|
| MIDI surface (polling) | <0.5% | ~5 MB | Low-frequency events |
| Stream Deck (15 key, rendering) | 2-5% | ~80 MB | Per-key image rendering |
| Stream Deck + (full LCD) | 3-8% | ~120 MB | Full-screen compositing |
| Gamepad (polling at 250 Hz) | <0.5% | ~3 MB | Lightweight |
| OSC surface (network) | <0.5% | ~5 MB | Event-driven |
| Multiple surfaces (5 devices) | 5-15% | ~300 MB | Cumulative |

### Endpoints in Pipeline Management

Endpoint devices MUST participate in the same pipeline lifecycle as
video/audio/HID.

#### Warm pipelines for screens

A Stream Deck's rendering pipeline SHOULD be kept warm -- the renderer process
stays running, frame buffer allocated, widget state cached. On scenario switch,
only the content changes, not the pipeline. This is important because Stream
Deck image upload is relatively slow over USB HID.

#### Activation time

Different endpoints have different activation times. The router MUST account
for these when planning pipeline activation:

| Endpoint | cold-to-warm | warm-to-active | Notes |
|----------|-----------|-------------|-------|
| WLED strip (UDP) | ~50ms | ~0ms | mDNS discovery + first frame |
| Stream Deck (USB) | 500-2000ms | <10ms | USB enumeration + device init |
| MIDI surface (USB) | 200-500ms | <10ms | ALSA/CoreMIDI device open |
| e-ink screen (SPI) | 100ms | ~5000ms | Fast init, slow panel refresh |
| OSC surface (network) | ~10ms | ~0ms | UDP socket, no handshake |
| Art-Net fixture (network) | ~10ms | ~0ms | UDP broadcast |

#### Degradation

When the controller is under resource pressure, endpoint pipelines MUST be
degraded before primary KVM pipelines. The following degradation order MUST
be applied:

1. Reduce screen refresh rate (30 fps to 10 fps to on-change-only)
2. Reduce RGB effect framerate (60 fps to 30 fps to 15 fps)
3. Simplify RGB effects (spatial to per-device solid)
4. Reduce screen rendering quality (JPEG quality, resolution)
5. Pause non-essential screens entirely (keep primary status screen)
6. HID/audio/video pipelines MUST NOT be degraded to save endpoint resources

This degradation order is automatic -- the router MUST apply it based on
resource pressure and pipeline priority. Endpoint pipelines MUST always have
lower priority than KVM pipelines unless explicitly overridden.

#### Capacity limits

An SBC node (Pi 5) might be able to drive 2 WLED strips and render 1 Stream
Deck screen simultaneously, but adding a third WLED strip and a second Stream
Deck would push it over capacity. The router MUST know this from the device
capacity model and MUST reject or degrade pipelines that exceed the node's
limits.
