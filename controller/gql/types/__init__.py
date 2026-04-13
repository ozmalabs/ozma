# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for Ozma Controller.

This module provides Strawberry GraphQL types and resolvers for:
- Audio routing (AudioRoute, AudioVolume)
- VBAN streams (VBANStream)
- Video streams (StreamInfo, CameraInfo)
- Control surfaces (ControlSurface, Binding)
- System health (SystemHealth)
"""

from .audio import AudioRoute, AudioVolume
from .vban import VBANStream
from .stream import StreamInfo, CameraInfo, CameraStream
from .controls import (
    ControlSurface,
    Binding,
    ControlBinding,
    Control,
    DisplayBinding,
    Display,
)
from .system import SystemHealth

__all__ = [
    "AudioRoute",
    "AudioVolume",
    "VBANStream",
    "StreamInfo",
    "CameraInfo",
    "CameraStream",
    "ControlSurface",
    "Binding",
    "ControlBinding",
    "Control",
    "DisplayBinding",
    "Display",
    "SystemHealth",
]
