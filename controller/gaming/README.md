# Ozma Gaming Module

Native Moonlight protocol implementation for game streaming with multi-user isolation and hybrid streaming support.

## Quick Start

```python
from controller.gaming import (
    moonlight_protocol,
    gstreamer_pipeline,
    moonlight_server,
    multi_user_streaming,
)

# Initialize protocol server
protocol = moonlight_protocol.MoonlightProtocolServer()
await protocol.start()

# Initialize pipeline manager
pipeline = gstreamer_pipeline.GStreamerPipelineManager()
await pipeline.start()

# Initialize server
server = moonlight_server.MoonlightServer()
await server.start()

# Initialize multi-user manager
manager = multi_user_streaming.MultiUserStreamingManager(protocol)
await manager.start()

# Create a session
client = protocol.create_session(client, client_addr)
session = await manager.create_session("session1", "user1", client, client_addr)
await manager.start_streaming(session)
```

## Architecture

```
controller.gaming/
├── __init__.py                  # Module exports
├── README.md                    # This file
├── moonlight_protocol.py        # RTSP+RTP+ENET protocol
├── moonlight_input.py           # Input protocol decoder
├── gstreamer_pipeline.py        # GStreamer encoding pipeline
├── moonlight_server.py          # Moonlight app server
├── headless_wayland.py          # Virtual Wayland compositor
├── virtual_input.py             # Per-session input devices
├── virtual_audio.py             # Per-session audio sinks
├── multi_user_streaming.py      # Multi-user session manager
├── scenario_app_mapping.py      # Scenario to app mapping
├── hybrid_streaming.py          # Unified source adapter
├── capture_to_moonlight.py      # HDMI capture integration
├── app_containers.py            # Docker/Podman isolation
├── profiles.py                  # User profiles with PIN
├── per_app_state.py             # Persistent game state
└── wolf_ui.py                   # In-stream app launcher
```

## Protocol Ports

| Port Range | Protocol | Description |
|------------|----------|-------------|
| 47990-47991 | HTTPS | Pairing (PIN exchange, cert pinning) |
| 47992-47993 | RTSP | Session negotiation |
| 47994+ | RTP | Video stream (H.264/H.265/AV1) |
| 47996+ | RTP | Audio stream (Opus) |
| 47998+ | ENET | Input control channel |

## Features

- **Native Moonlight Protocol**: Full RTSP+RTP+ENET implementation
- **Multiple Codecs**: H.264, H.265/HEVC, AV1
- **Hardware Encoding**: NVENC, VAAPI, QuickSync, V4L2 M2M
- **Multi-User**: Concurrent isolated sessions
- **Hybrid Streaming**: Physical capture, VNC, virtual desktop
- **Container Support**: Docker/Podman game isolation
- **Input Isolation**: Per-session uinput devices
- **Audio Isolation**: Per-session PipeWire sinks

## Requirements

- Python 3.11+
- aiohttp
- GStreamer 1.18+
- libevdev
- pipewire
- Docker/Podman (optional, for container isolation)

## Configuration

### Pipeline Configuration

```python
from controller.gaming.gstreamer_pipeline import PipelineConfig

config = PipelineConfig(
    name="default",
    video_encoder=EncoderConfig(name="nvenc", codec="h265", bitrate_kbps=50000),
    sources=[SourceConfig(type="display", width=1920, height=1080, fps=60)],
    outputs=[OutputConfig(type="rtp", host="127.0.0.1", port=47994)],
)
```

### Container Configuration

```python
from controller.gaming.app_containers import ContainerConfig

config = ContainerConfig(
    app_id="steam-game",
    image="steam-game:latest",
    gpu=True,
    memory_limit="8G",
    cpu_limit="4",
)
```

## Testing

```bash
# Run all gaming tests
python -m pytest controller/gaming/tests/

# Run specific test
python -m pytest controller/gaming/tests/test_moonlight_protocol.py
```

## Integration

The gaming module integrates with:

- `scenarios.py`: Scenario management
- `audio.py`: Audio routing
- `hid.py`: HID input forwarding
- `auth.py`: Authentication and authorization

## See Also

- [Build Site V1.2 Moonlight](../../docs/build-site-v1.2-moonlight.md)
- [Architecture](../../docs/architecture.md)
