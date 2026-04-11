# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
V1.2 Game Streaming — Moonlight protocol implementation.

This module provides native Moonlight wire protocol support (RTSP+RTP+ENET)
for game streaming. It replaces the Sunshine subprocess model with a
full native implementation.

Architecture:

  controller.gaming.moonlight_protocol
    - Native HTTPS pairing (PIN exchange, cert pinning)
    - RTSP server (session negotiation)
    - RTP packetiser (H.265 + H.264 + AV1 with FEC)
    - ENET control channel (input, HDR, control messages)
    - AES-GCM per-session encryption

  controller.gaming.moonlight_input
    - Full Moonlight input protocol decode: keyboard, mouse, touch, pen, gyro, haptics
    - Per-client controller type override (PS/Xbox/Nintendo mapping)
    - Per-client mouse acceleration + scroll settings
    - Output: feeds existing evdev/HID injection pipeline

  controller.gaming.gstreamer_pipeline
    - GStreamer encode pipeline: VAAPI/NVENC/QuickSync → RTP packetiser → FEC
    - TOML/JSON configurable pipeline string (Wolf pattern)
    - Gamescope integration hook (XWayland + FSR + HDR — stubs OK at this stage)

  controller.gaming.moonlight_server
    - Controller presents scenarios as Moonlight app list
    - Each scenario = one "app" in Moonlight client
    - Pairing database (client cert pinning, session tokens)
    - Launch/quit app → activate/deactivate scenario

  controller.gaming.headless_wayland
    - Virtual Wayland compositor per session (wlroots or Smithay)
    - XWayland for legacy X11 apps
    - Virtual framebuffer (no physical display required)
    - One compositor instance per concurrent stream session

  controller.gaming.virtual_input
    - Per-session uinput/uhid devices (upgrade from shared evdev)
    - Gamepad hotplug simulation in containers (fake-udev pattern)
    - Session teardown cleans up uinput devices

  controller.gaming.virtual_audio
    - Per-session PipeWire sink (upgrade from shared PipeWire routing)
    - Session sink destroyed on disconnect
    - VBAN output still available per-session

  controller.gaming.multi_user_streaming
    - Concurrent isolated sessions (N users, N virtual desktops, N input/audio sets)
    - Session lifecycle: create → stream → pause → resume → destroy
    - Resource limits per session (configurable)

  controller.gaming.scenario_app_mapping
    - Map each scenario type to Moonlight app list entry:
      - Physical machine → HDMI capture → Moonlight RTP
      - VM → VNC → Moonlight RTP
      - Container → virtual desktop → Moonlight RTP
    - Switching Moonlight app = scenario switch

  controller.gaming.hybrid_streaming
    - Unified source adapter: physical capture card / VNC / virtual desktop → common frame source
    - GStreamer pipeline accepts any frame source
    - Single Moonlight server presents all source types

  controller.gaming.capture_to_moonlight
    - HDMI capture card (V4L2) → GStreamer → Moonlight RTP
    - Reuses display_capture.py capture pipeline
    - No HDCP issue (physical capture of own hardware)

  controller.gaming.app_containers
    - Docker/Podman game isolation
    - Per-app persistent home dir (auto-mount)
    - Fake-udev for gamepad hotplug in containers
    - GPU passthrough to container (render node)

  controller.gaming.profiles
    - User profiles with PIN lock
    - Custom app icons per profile
    - Per-profile app list (subset of all scenarios)

  controller.gaming.per_app_state
    - Persistent game saves per user per app
    - Auto-mount on session start, sync on exit

  controller.gaming.wolf_ui
    - In-stream app launcher overlay (Ctrl+Alt+Shift+W)
    - Navigate app list without exiting stream

See build-site-v1.2-moonlight.md for full specification.
"""

from __future__ import annotations

from pathlib import Path

# Import all submodules
from . import (
    app_containers,
    capture_to_moonlight,
    gstreamer_pipeline,
    headless_wayland,
    hybrid_streaming,
    moonlight_input,
    moonlight_protocol,
    moonlight_server,
    multi_user_streaming,
    per_app_state,
    profiles,
    scenario_app_mapping,
    virtual_audio,
    virtual_input,
    wolf_ui,
)

# Module-level constants
MODULE_DIR = Path(__file__).parent
GAMING_DIR = MODULE_DIR  # /controller/gaming

# Data directory for persistent state
DATA_DIR = Path("/var/lib/ozma/gaming")
DATA_DIR.mkdir(parents=True, exist_ok=True)

__all__ = [
    "app_containers",
    "capture_to_moonlight",
    "gstreamer_pipeline",
    "headless_wayland",
    "hybrid_streaming",
    "moonlight_input",
    "moonlight_protocol",
    "moonlight_server",
    "multi_user_streaming",
    "per_app_state",
    "profiles",
    "scenario_app_mapping",
    "virtual_audio",
    "virtual_input",
    "wolf_ui",
]
