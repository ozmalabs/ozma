# Node Definition

**Status:** Draft
**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document defines the Node Definition schema for the Ozma routing
specification. A node is the fundamental building block of the Ozma mesh. Every
node MUST be described as a compound device comprising its hardware platform, USB
gadget presentation, physical I/O buses, software services, network membership,
cryptographic identity, and binding to a single target machine. This document
specifies each of these components, their schemas, their constraints, and their
relationship to the routing graph.

---

## 1. Node as a Composite Device

Every node in the Ozma mesh MUST have a `NodeDefinition`. The definition
describes the node's complete hardware composition, what it presents to its
target machine, and the ports it contributes to the routing graph.

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

A `NodeDefinition` MUST include all of the above fields. The `id` field MUST
be globally unique and MUST be assigned during enrollment (see Section 8).

---

## 2. Platform

The `PlatformSpec` describes the hardware platform the node runs on. Implementations
SHOULD populate all known fields. Fields marked with `?` are OPTIONAL.

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

The `hardware`, `arch`, `cpu_cores`, `memory_mb`, and `usb_otg` fields are
REQUIRED. A node that presents a USB gadget to its target MUST have
`usb_otg: true`.

---

## 3. Target Binding

A node MUST be **permanently wired** to one target machine. This relationship
is the fundamental invariant of the Ozma architecture: switching active focus
changes only the network routing, never the physical USB connection. The target
binding MUST declare the connection type.

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

The `connection_type` field is REQUIRED and MUST be one of `"usb_cable"`,
`"usb_c"`, `"internal_header"`, or `"virtual"`. The `gadget_ref` field is
REQUIRED and MUST reference a valid `GadgetSpec`. The `lom` field is OPTIONAL.

### 3.1 Lights-out Management (LomSpec)

The `LomSpec` defines physical control over the target machine's power state.
When present, implementations MUST respect the `hold_ms` timing for button
presses.

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

If `wake_on_lan` is `true`, the `wake_on_lan_mac` field MUST be present.
If `bmc` is present, `bmc_address` MUST also be present.

---

## 4. USB Gadget Specification

The gadget spec defines the composite USB device the node presents to the
target machine. This is what the target's OS sees when it enumerates the
node's USB port. The USB gadget MUST present at least one HID function.

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

The `functions` array MUST NOT be empty. At least one function MUST have a
`type` beginning with `hid_`. The `serial_number` MUST be unique across all
nodes in a mesh.

### 4.1 Gadget Function Types

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

Implementations MAY support additional function types not listed here, but
all implementations MUST support `hid_keyboard` and `hid_mouse`.

### 4.2 HID Function Configuration

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

A HID function MUST provide either `report_descriptor` or
`report_descriptor_template`, but MUST NOT provide both.

### 4.3 UAC2 Function Configuration

```yaml
Uac2FunctionConfig:
  direction: string             # "playback" (target->node), "capture" (node->target)
  sample_rate: uint             # Hz
  channels: uint                # 1=mono, 2=stereo
  bit_depth: uint               # 16, 24, 32
  channel_mask: uint?           # channel position mask
```

When a UAC2 function is present, the `direction` field MUST be one of
`"playback"` or `"capture"`.

### 4.4 UVC Function Configuration

```yaml
UvcFunctionConfig:
  resolutions: Resolution[]     # supported resolutions
  framerates: float[]           # supported framerates
  format: string                # "mjpeg", "yuyv", "nv12"
  max_payload_size: uint?       # max frame size in bytes
```

### 4.5 Device Emulation

```yaml
VendorFunctionConfig:
  mode: string                  # "clone", "passthrough", "emulate"
  profile: string?              # device database entry ID or USB profile ID
  source_device: DeviceRef?     # for clone/passthrough: the real device to clone from
  descriptor_tree: UsbDescriptorTree?  # full USB descriptor tree (from profile or captured)
  intercept_endpoints: EndpointInterceptRule[]?  # which endpoints to intercept vs forward
```

When `mode` is `"clone"` or `"passthrough"`, the `source_device` field MUST
be present. When `mode` is `"emulate"`, either `profile` or `descriptor_tree`
MUST be provided.

The gadget spec lets you define exactly what the target machine sees. A
minimal node presents just HID keyboard + mouse. A full node adds UAC2
audio, UVC camera, and possibly a cloned vendor device for RGB passthrough.

---

## 5. Physical I/O Buses

Nodes have physical I/O beyond USB. These connect sensors, actuators, LEDs,
and screens. Implementations SHOULD enumerate all connected peripherals.

```yaml
PeripheralBus:
  type: string                  # "gpio", "i2c", "spi", "uart", "pwm", "onewire"
  bus_id: string                # bus identifier ("i2c-1", "spidev0.0", "gpio")
  devices: BusDevice[]          # what's connected to this bus

BusDevice:
  type: string                  # device type from the device taxonomy
  device_db_id: string?         # device database entry
  address: string?              # bus address (I2C: "0x40", SPI: "CS0", GPIO: "pin17")
  driver: string?               # driver or plugin ID
  port_mappings: PortMapping[]  # maps bus device to routing graph ports

PortMapping:
  bus_endpoint: string          # endpoint on the bus device
  graph_port: PortRef           # port in the routing graph
```

Each `BusDevice` MUST have at least one entry in `port_mappings` so that it
is addressable from the routing graph.

**Example -- a complete hardware node's peripheral bus configuration**:

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

---

## 6. Node Services

Software services running on the node create ports in the routing graph. Each
service MUST declare the ports it creates.

```yaml
NodeService:
  id: string                    # service identifier
  type: string                  # service type
  enabled: bool
  config: ServiceConfig         # type-specific configuration
  ports: PortRef[]              # ports this service creates in the graph
  resource_cost: ResourceCost   # resources this service consumes
```

A node MUST run at least the `hid_udp_receiver` service to participate in
HID routing.

### 6.1 Standard Node Services

| Service | Type | Ports created | Description |
|---------|------|--------------|-------------|
| HID receiver | `hid_udp_receiver` | HID sink (UDP) | Receives HID from controller |
| Video capture | `video_capture` | Video source | V4L2 capture + encode |
| Audio bridge | `audio_bridge` | Audio source + sink | UAC2 <-> VBAN/Opus |
| RGB controller | `rgb_controller` | RGB sink | Drives addressable LEDs |
| mDNS announcer | `mdns` | -- | Service discovery |
| Binary channel | `binary_channel` | Control port | Event/command channel |
| HTTP server | `http_server` | -- | Serves streams and API |
| Self-management | `self_management` | Data source | CPU/temp/memory metrics |
| Connect client | `connect_client` | -- | Registration, relay, heartbeat |
| RPA engine | `node_rpa` | Control port | OCR + BIOS navigation |
| Serial capture | `serial_capture` | Data source | Serial console output |
| Phone endpoint | `phone_endpoint` | Audio source + sink | Phone UAC2 bridge |

---

## 7. Network and Mesh Membership

Every node MUST have at least one network interface. A node that is enrolled
in a mesh MUST have a WireGuard overlay interface.

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

The `enrollment_state` field MUST be one of `"unenrolled"`, `"pending"`,
`"enrolled"`, or `"revoked"`. A node with `enrollment_state: "revoked"`
MUST NOT participate in routing.

---

## 8. Node Identity and Enrollment

Node identity MUST use Ed25519 keypairs. The identity key is generated at
first boot and MUST NOT change for the lifetime of the node.

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

The `node_id` MUST be globally unique. Implementations SHOULD derive it from
the identity key. The `identity_key` MUST be an Ed25519 keypair. The private
key MUST be stored securely on the node and MUST NOT be transmitted.

The `state` field MUST be one of `"factory_new"`, `"enrolled"`, `"orphaned"`,
or `"revoked"`. An `"orphaned"` node is one whose controller is no longer
reachable; it MAY re-enroll with a new controller.

---

## 9. Node Lifecycle

A node MUST progress through the following lifecycle stages in order. Each
stage maps to spec primitives defined in the preceding sections.

```
Power on
  -> Hardware init (GPIO, I2C, SPI setup)
  -> USB gadget creation (ConfigFS -- present HID/UAC2/UVC to target)
  -> Network init (DHCP, WireGuard tunnel)
  -> Service startup (HID receiver, video capture, audio bridge, etc.)
  -> Discovery (mDNS announcement OR direct registration with controller)
  -> Enrollment (key exchange with controller, join mesh)
  -> Operational (ready to receive HID, stream video, etc.)
  -> Graph participation (controller adds node's ports to routing graph)
```

| Lifecycle stage | Spec primitive |
|----------------|---------------|
| Hardware init | PeripheralBus setup (Section 5) |
| USB gadget | GadgetSpec activation (Section 4), creates ports |
| Network init | NetworkInterface, transport plugin setup |
| Service startup | NodeService activation (Section 6), creates ports |
| Discovery | Topology discovery layer 3 |
| Enrollment | NodeIdentity (Section 8), security model |
| Operational | All ports active, pipelines can be assembled |
| Graph participation | Device + ports appear in controller's routing graph |

A node MUST NOT participate in routing until it has reached the "Operational"
stage. The controller MUST NOT add a node to the routing graph until it has
verified the node's identity and enrollment state.

---

## 10. Worked Example -- Defining a Hardware Node

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

This definition is complete -- everything about the node's hardware, what it
presents to the target, its physical I/O, its services, its network identity,
and its ports in the routing graph is expressed in spec terms. A different
node (e.g., a Raspberry Pi Zero 2 W with no capture card) would have a
different `platform`, a simpler `gadget` (just HID), no `video_capture`
service, and fewer peripherals -- but the same schema.
