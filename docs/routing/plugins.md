# Plugin Contracts

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

Ozma's routing system is extended exclusively through plugins. Transports,
devices, codecs, converters, and switches are all plugin types with defined
interfaces. This document specifies the plugin registration and lifecycle
model, and the contracts for device, codec, converter, and switch plugin types.
Built-in plugins MUST use the same interface as third-party plugins -- there is
no distinction between "core" and "external" at the interface level.

## Specification

### Registration and lifecycle

Plugins are the extension mechanism for the entire routing system. The built-in
transports, device plugins, and codecs that ship with Ozma are themselves
plugins -- they use the same registration and lifecycle as third-party plugins.

**Plugin manifest**:

Every plugin MUST provide a manifest describing its identity, type, and
requirements.

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
  transport                     # moves data between ports (see transports.md)
  device                        # discovers hardware/software devices (SS 6.2)
  codec                         # encodes/decodes media (SS 6.3)
  converter                     # transforms data between formats (SS 6.4)
  switch                        # controls external switching devices (SS 6.5)
  composite                     # provides multiple of the above (e.g., a plugin
                                # that discovers LoRa devices AND provides LoRa transport)
```

**Plugin lifecycle**:

```
discover -> load manifest -> check compatibility -> instantiate -> register -> start
  |                                                                         |
  |  On shutdown or unload:                                                |
  +-- stop -> deregister -> unload                                         |
                                                                           |
  Running:                                                                 |
  +-- graph queries (discover_links, capabilities, etc.)                   |
  +-- active operations (open, close, measure)                             |
  +-- events (on_link_change, on_hotplug, on_state_change)                 |
```

**Registration methods**:

1. **Built-in**: Ship with Ozma. Loaded automatically on startup. Built-in
   plugins MUST NOT be unloaded.

2. **Installed**: Installed via `pip install` or placed in the plugins
   directory. Loaded on startup if present. MAY be disabled via config.
   ```
   pip install ozma-plugin-lora
   # or
   cp lora_transport.py ~/.config/ozma/plugins/
   ```

3. **Dynamic**: Loaded at runtime via the API. Useful for development and
   testing. MAY be loaded and unloaded without restart.
   ```
   POST /api/v1/plugins/load    { "entry_point": "my_plugin:MyTransport" }
   DELETE /api/v1/plugins/{id}   # unload
   ```

**Plugin isolation**: Plugins run in the controller's process but are
sandboxed by convention -- they MUST only interact with the system through
the plugin interface methods. The plugin host MUST catch crashes, hangs, and
excessive resource use from misbehaving plugins, log the failure, and disable
the offending plugin. The controller MUST continue operating without it.

**Language stability guarantee**: The Python plugin interface MUST remain
stable across implementation language changes. This is a stable contract --
plugins written in Python today MUST continue to work if the controller core
is rewritten in another language (e.g., Rust via PyO3 embedded interpreter).
The Python plugin interface is the public API; the controller's implementation
language is an internal detail. Plugin authors SHOULD NOT depend on the
controller being Python -- only on the plugin interface classes and the graph
query API they expose.

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

**What a plugin MAY do**:

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

**What a plugin MUST NOT do**:

- Modify other plugins' state
- Bypass the transport encryption model
- Access the filesystem outside its data directory
- Register core API endpoints (only namespaced under `/api/v1/plugins/{id}/`)
- Override built-in plugin behaviour (but MAY provide alternatives)

**Example -- a LoRa transport plugin**:

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
characteristics, and participate in routing decisions. The router does not know
or care that it came from a plugin -- it is just another transport with a cost.

### Transport plugin

See [transports.md](transports.md) for the transport plugin contract and
transport-specific models.

### Device plugin

A device plugin makes a class of hardware or software discoverable to the
graph. Plugins MUST implement the methods defined in the `DevicePlugin`
interface.

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

### Codec plugin

A codec plugin handles encoding and decoding of media data. Plugins MUST
implement the methods defined in the `CodecPlugin` interface.

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

### Converter plugin

A converter transforms data between formats without being a full codec.
Examples: pixel format conversion, sample rate conversion, channel remixing,
HID report translation. Plugins MUST implement the methods defined in the
`ConverterPlugin` interface.

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

### Switch plugin

A switch plugin controls an external switching device (KVM switch, HDMI matrix,
audio matrix, AV receiver, etc.). Switch plugins bridge between the graph model
and the device's control interface. Plugins MUST implement the methods defined
in the `SwitchPlugin` interface.

```yaml
SwitchPlugin:
  id: string                    # "tesmart-serial", "hdmi-cec", "extron-ip",
                                # "ir-blaster", "manual"
  name: string
  media_types: MediaType[]      # what this switch routes (video, audio, hid, mixed)
  controllability: Controllability

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
| Serial (RS-232/USB) | Device-specific command set | Varies -- some echo state, some are silent | TESmart, Extron, Crestron, many pro AV |
| IP (TCP/UDP) | HTTP API, Telnet, or proprietary | Usually confirmed | Extron, Blackmagic, enterprise AV |
| IR blaster | Consumer IR codes | Write-only -- no feedback channel | Cheap HDMI switches, AV receivers |
| HDMI CEC | CEC protocol over HDMI pin 13 | Confirmed (CEC has acknowledgement) | TVs, AV receivers, some monitors |
| USB HID | Vendor-specific HID reports | Varies by device | Some USB KVM switches |
| Manual | N/A | User confirmation only | Physical button switches |

**Write-only devices and verification**: When the router activates a pipeline
through a write-only switch, it cannot confirm the switch state directly. But
it MAY verify indirectly: if video or audio starts flowing through the expected
path after the switch command, the state is implicitly confirmed and upgraded
from `commanded` to `measured`. If no data flows within a timeout, the router
SHOULD treat the switch state as `assumed` (possibly wrong) and MAY retry or
alert.
