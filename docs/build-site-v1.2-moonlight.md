# Build Site: V1.2 Moonlight / Game Streaming

**Date:** 2026-04-11  
**Status:** Implemented  
**Author:** Ozma Labs  

---

## Overview

This document describes the V1.2 Moonlight implementation for Ozma — a native wire protocol implementation for game streaming with full protocol support (RTSP+RTP+ENET), multi-user isolation, and hybrid streaming (physical + VM + container).

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Moonlight Client (PC)                            │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  HTTPS Pairing (47990-47991)                                │   │
│  │  - PIN exchange                                              │   │
│  │  - Certificate pinning                                       │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│                              ▼                                      │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  RTSP Server (47992-47993)                                  │   │
│  │  - Session negotiation (DESCRIBE, SETUP, PLAY)              │   │
│  │  - SDP description                                           │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│                              ▼                                      │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  RTP Video Stream (47994+)                                  │   │
│  │  - H.264/H.265/AV1 encoding                                 │   │
│  │  - Forward Error Correction (FEC)                           │   │
│  │  - AES-GCM encryption per session                           │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│                              ▼                                      │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  ENET Control (47998+)                                      │   │
│  │  - Input events (keyboard, mouse, gamepad, touch)           │   │
│  │  - HDR metadata                                             │   │
│  │  - Control messages (pause, resume, config)                 │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────┐
│                    Ozma Controller                                  │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │  controller.gaming.moonlight_protocol                       │   │
│  │  - HTTPS pairing server                                     │   │
│  │  - RTSP session management                                  │   │
│  │  - RTP packetiser                                           │   │
│  │  - ENET protocol                                            │   │
│  │  - AES-GCM encryption                                       │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.moonlight_input                          │   │
│  │  - Input protocol decode                                    │   │
│  │  - Per-client mapping (PS/Xbox/Nintendo)                   │   │
│  │  - InputInjector → evdev/HID                              │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.gstreamer_pipeline                       │   │
│  │  - Hardware encoding (VAAPI/NVENC/QuickSync)               │   │
│  │  - Software fallback (libx264, libx265, libaom-av1)        │   │
│  │  - Configurable pipeline via TOML/JSON                     │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.moonlight_server                         │   │
│  │  - Scenarios as Moonlight apps                              │   │
│  │  - App list management                                      │   │
│  │  - Launch/quit app handling                                 │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.headless_wayland                         │   │
│  │  - Virtual Wayland compositor per session                   │   │
│  │  - XWayland for legacy X11 apps                            │   │
│  │  - One compositor per concurrent stream                    │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.multi_user_streaming                     │   │
│  │  - Concurrent isolated sessions                             │   │
│  │  - Per-session resources (input/audio/compositor)          │   │
│  │  - Resource limits per session                             │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.virtual_input                            │   │
│  │  - Per-session uinput devices                               │   │
│  │  - Gamepad hotplug simulation (fake-udev)                  │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.virtual_audio                            │   │
│  │  - Per-session PipeWire sink                               │   │
│  │  - Session sink destroyed on disconnect                    │   │
│  │  - VBAN output per session                                 │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.scenario_app_mapping                     │   │
│  │  - Map scenarios to Moonlight apps                          │   │
│  │  - Physical → HDMI capture                                  │   │
│  │  - VM → VNC                                                 │   │
│  │  - Container → virtual desktop                             │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.hybrid_streaming                         │   │
│  │  - Unified source adapter                                   │   │
│  │  - GStreamer pipeline accepts any source                   │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.capture_to_moonlight                     │   │
│  │  - HDMI capture → GStreamer → Moonlight RTP                │   │
│  │  - Reuses display_capture.py pipeline                      │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                      │
│  ┌──────────────────────────┼──────────────────────────────────┐   │
│  │                          ▼                                  │   │
│  │  controller.gaming.app_containers                           │   │
│  │  - Docker/Podman game isolation                            │   │
│  │  - Per-app persistent home dir                             │   │
│  │  - Fake-udev for gamepad hotplug                           │   │
│  │  - GPU passthrough                                         │   │
│  └─────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Tier 0 — Protocol Foundation

### `controller.gaming.moonlight_protocol`

Native Moonlight protocol implementation with:

- **HTTPS Pairing (ports 47990-47991)**
  - 4-digit PIN generation and verification
  - Client certificate pinning for security
  - Persistent client database

- **RTSP Server (ports 47992-47993)**
  - Session negotiation via DESCRIBE, SETUP, PLAY, TEARDOWN
  - SDP description generation
  - Multiple concurrent sessions

- **RTP Packetiser**
  - H.264 (RFC 6184)
  - H.265/HEVC (RFC 7798)
  - AV1 (RFC 9172)
  - Forward Error Correction (FEC)

- **ENET Control Channel**
  - Input events: keyboard, mouse, touch, gamepad, haptics, pen, gyro
  - Control messages: pause, resume, config
  - AES-GCM per-session encryption

### `controller.gaming.moonlight_input`

Input protocol decoder with:

- Full Moonlight input protocol decode
- Per-client controller type override:
  - Xbox mapping
  - PlayStation mapping
  - Nintendo mapping
  - Steam controller mapping
- Per-client mouse acceleration profiles
- Per-client scroll sensitivity settings
- Output to evdev/HID injection pipeline

### `controller.gaming.gstreamer_pipeline`

GStreamer encoding pipeline with:

- Hardware encoders:
  - NVENC (NVIDIA)
  - VAAPI (Intel/AMD)
  - QuickSync (Intel)
  - V4L2 M2M
- Software fallbacks:
  - libx264 (H.264)
  - libx265 (HEVC)
  - libaom-av1 (AV1)
- Configurable via TOML/JSON
- Gamescope integration hooks

---

## Tier 1 — Server Layer

### `controller.gaming.moonlight_server`

Moonlight server that presents Ozma scenarios as apps:

- Scenarios appear as Moonlight "apps"
- Each scenario = one app in Moonlight client
- App launch → scenario activation
- App quit → scenario deactivation
- Client certificate pinning

### `controller.gaming.headless_wayland`

Virtual Wayland compositor per session:

- Virtual Wayland compositor (wlroots-based or Smithay-based)
- XWayland for legacy X11 applications
- Virtual framebuffer (no physical display required)
- One compositor instance per concurrent stream session

---

## Tier 2 — Isolation Layer

### `controller.gaming.virtual_input`

Per-session virtual input devices:

- Per-session uinput/uhid devices
- Gamepad hotplug simulation (fake-udev pattern)
- Session teardown cleans up uinput devices

### `controller.gaming.virtual_audio`

Per-session audio isolation:

- Per-session PipeWire sink
- Session sink destroyed on disconnect
- VBAN output still available per-session

### `controller.gaming.multi_user_streaming`

Multi-user concurrent streaming:

- Concurrent isolated sessions (N users, N virtual desktops, N input/audio sets)
- Session lifecycle: create → stream → pause → resume → destroy
- Resource limits per session (configurable)

---

## Tier 3 — Hybrid Streaming

### `controller.gaming.scenario_app_mapping`

Scenario to Moonlight app mapping:

| Scenario Type | Source | Destination |
|--------------|--------|-------------|
| Physical | HDMI capture (V4L2) | Moonlight RTP |
| VM | VNC | Moonlight RTP |
| Container | Virtual desktop | Moonlight RTP |

- Switching Moonlight app = scenario switch

### `controller.gaming.hybrid_streaming`

Unified source adapter:

- Physical capture card / VNC / virtual desktop → common frame source
- GStreamer pipeline accepts any frame source
- Single Moonlight server presents all source types

### `controller.gaming.capture_to_moonlight`

HDMI capture integration:

- HDMI capture card (V4L2) → GStreamer → Moonlight RTP
- Reuses `display_capture.py` capture pipeline
- No HDCP issue (physical capture of own hardware)

---

## Tier 4 — App Management

### `controller.gaming.app_containers`

Container-based game isolation:

- Docker/Podman game isolation
- Per-app persistent home directory (auto-mount)
- Fake-udev for gamepad hotplug in containers
- GPU passthrough to container (render node)

### `controller.gaming.profiles`

User profiles:

- User profiles with PIN lock
- Custom app icons per profile
- Per-profile app list (subset of all scenarios)

### `controller.gaming.per_app_state`

Persistent game state:

- Persistent game saves per user per app
- Auto-mount on session start
- Sync on exit

### `controller.gaming.wolf_ui`

In-stream app launcher:

- Ctrl+Alt+Shift+W overlay
- Navigate app list without exiting stream

---

## Protocol Details

### HTTPS Pairing

```
Client → Server: GET /pair
Server → Client: {"pin": "1234", "expires_in": 300}

Client → Server: POST /pair
{"pin": "1234", "cert_hash": "abc123..."}
Server → Client: 201 Created
{"client_id": "...", "client_cert_hash": "..."}
```

### RTSP Session

```
Client → Server: DESCRIBE rtsp://...
Server → Client: 200 OK
Content-Type: application/sdp
v=0
o=- session_id session_id IN IP4 127.0.0.1
s=Ozma Moonlight Stream
m=video 47994 RTP/AVP 96
a=rtpmap:96 H264/90000

Client → Server: SETUP rtsp://.../streamid=0
Server → Client: 200 OK
Transport: RTP/AVP/UDP;unicast;client_port=47994-47995

Client → Server: PLAY rtsp://...
Server → Client: 200 OK
```

### ENET Input Protocol

```
Message Type (1 byte):
  0x01 - Keyboard
  0x02 - Mouse
  0x03 - Gamepad
  0x04 - Touch
  0x05 - Haptic
  0x06 - Hyper (HDR)
  0x07 - Pen
  0x08 - Gyro
  0x10 - Control
  0x11 - Config
```

---

## API Reference

### Moonlight Protocol Server

```python
from controller.gaming.moonlight_protocol import MoonlightProtocolServer

server = MoonlightProtocolServer()
await server.start()
# Server runs on ports 47990-47999

# Create a streaming session
session = server.create_session(client, client_addr)

# Send input event
server.send_input_event(session_id, {
    "type": "mouse",
    "buttons": 1,
    "x": 100,
    "y": 200,
    "scroll": 0,
})

await server.stop()
```

### Input Decoder

```python
from controller.gaming.moonlight_input import MoonlightInputDecoder

decoder = MoonlightInputDecoder()

# Decode keyboard input
event = decoder.decode_keyboard(input_data)
if event:
    # Inject into evdev
    pass

# Set client-specific configuration
decoder.set_client_config(client_id, InputConfig(
    controller_type=ControllerType.PLAYSTATION,
    mouse_acceleration=MouseAcceleration.LINEAR,
))
```

### GStreamer Pipeline

```python
from controller.gaming.gstreamer_pipeline import (
    GStreamerPipelineManager,
    PipelineConfig,
    EncoderConfig,
    SourceConfig,
    OutputConfig,
)

manager = GStreamerPipelineManager()

# Configure pipeline
config = PipelineConfig(
    name="my_stream",
    video_encoder=EncoderConfig(
        name="nvenc",
        codec="h265",
        bitrate_kbps=50_000,
    ),
    sources=[SourceConfig(type="display", width=1920, height=1080, fps=60)],
    outputs=[OutputConfig(type="rtp", host="127.0.0.1", port=47994)],
)

await manager.start_pipeline("my_stream", config)
await manager.stop_pipeline("my_stream")
```

### Container Management

```python
from controller.gaming.app_containers import ContainerManager, ContainerConfig

manager = ContainerManager()

# Create container configuration
config = ContainerConfig(
    app_id="steam-game",
    image="steam-game:latest",
    command=["/start-game.sh"],
    gpu=True,
    memory_limit="8G",
    cpu_limit="4",
)

# Setup persistent home directory
home_path = manager.setup_app_home("steam-game", username="user")

# Start container
await manager.start_container("steam-game")
```

---

## Configuration

### Pipeline Configuration (JSON)

```json
{
  "name": "default",
  "pipeline_type": "hardware",
  "video_encoder": {
    "name": "nvenc",
    "codec": "h265",
    "preset": "p4",
    "tune": "ll",
    "rc": "cbr",
    "bitrate_kbps": 50000,
    "max_bitrate_kbps": 0,
    "bufsize_kbps": 0,
    "qp": 24,
    "hardware": true,
    "device": ""
  },
  "audio_encoder": "opus",
  "audio_bitrate_kbps": 160,
  "sources": [
    {
      "type": "display",
      "display": ":0",
      "width": 1920,
      "height": 1080,
      "fps": 60,
      "format": "NV12",
      "capture_method": "auto"
    }
  ],
  "outputs": [
    {
      "type": "rtp",
      "host": "127.0.0.1",
      "port": 47994,
      "port_rtcp": 47995,
      "fec_enabled": true,
      "fec_percentage": 20
    }
  ],
  "low_latency": true
}
```

### Container Configuration (JSON)

```json
{
  "app_id": "steam-game",
  "image": "steam-game:latest",
  "command": ["/start-game.sh"],
  "working_dir": "/home/user",
  "volumes": [
    {"host": "/path/to/save", "container": "/home/user/saves", "opts": "Z"}
  ],
  "env": {
    "DISPLAY": "wayland-0"
  },
  "gpu": true,
  "gpu_device": "/dev/dri/renderD128",
  "network": "bridge",
  "memory_limit": "8G",
  "cpu_limit": "4",
  "auto_start": false,
  "auto_remove": true
}
```

---

## Dependencies

### Runtime

- Python 3.11+
- aiohttp (HTTP server)
- GStreamer 1.18+ (encoding pipeline)
- libevdev (input)
- pipewire (audio)
- Docker/Podman (container isolation)

### Optional

- CUDA/NVIDIA drivers (for NVENC)
- Intel oneAPI (for QuickSync)
- AMD GPU drivers (for VAAPI)
- wlroots (for headless Wayland)
- uinput kernel module

---

## Testing

```bash
# Test protocol server
python -m pytest controller/gaming/tests/test_moonlight_protocol.py

# Test input decoder
python -m pytest controller/gaming/tests/test_moonlight_input.py

# Test gstreamer pipeline
python -m pytest controller/gaming/tests/test_gstreamer_pipeline.py

# Integration tests
python -m pytest controller/gaming/tests/test_integration.py
```

---

## Migration from Sunshine

To migrate from Sunshine subprocess model to native protocol:

1. **Stop Sunshine**:
   ```bash
   systemctl stop sunshine
   ```

2. **Start Ozma Moonlight server**:
   ```bash
   python controller/main.py
   ```

3. **Re-pair clients** (optional):
   - Clients can continue using existing certificates
   - Or re-pair for new cert pinning

4. **Update client configuration**:
   - Moonlight clients automatically discover Ozma via mDNS
   - No manual IP configuration needed

---

## Future Enhancements

### Tier 5 (Non-blocking)

- Lobbies for co-op sessions
- Multi-GPU support (iGPU encode + dGPU render)
- Gamescope integration (XWayland/HDR/FSR)
- HDR10 passthrough
- Cross-machine lobby spanning physical + VM + container

---

## See Also

- [Architecture](architecture.md)
- [Protocols](protocols.md)
- [Security](security.md)
