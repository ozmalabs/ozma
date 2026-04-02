# Ozma Soft Node

> **Easy things automatic. Hard things easy. Amazing things possible.**

Make any PC appear as an ozma node. No hardware required.

## Install

```bash
uv pip install ozma-softnode
```

## Run

```bash
# Basic — auto-discovers controller on the network
ozma-softnode --name my-desktop

# With explicit controller
ozma-softnode --name my-desktop --controller http://10.0.0.1:7380

# Without screen capture (headless server)
ozma-softnode --name my-server --no-capture
```

The node appears in the ozma dashboard immediately. Switch to it from any scenario.

## What it does

- **HID injection** — the controller sends keyboard/mouse input, the soft node injects it into the local input system (uinput on Linux)
- **Audio routing** — creates a virtual PipeWire audio sink that the controller routes to/from
- **Screen capture** — captures the display as an HLS stream (x11grab, PipeWire, or platform-native)
- **mDNS discovery** — auto-discovered by the controller, zero configuration

## Requirements

- Python 3.11+
- Linux (uinput for HID, PipeWire/PulseAudio for audio, X11/Wayland for capture)
- macOS and Windows support is planned (currently HID injection is Linux-only)

## License

AGPL-3.0 with plugin exception. See COPYING in the ozma repo.
