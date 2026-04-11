# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types package.
"""

from .nodes import NodeType, NodeInfoType, DisplayOutputType, CameraStreamType
from .scenarios import (
    ScenarioType,
    BindingType,
    TransitionConfigType,
    MotionPresetType,
    BluetoothConfigType,
    WallpaperConfigType,
)

__all__ = [
    "NodeType",
    "NodeInfoType",
    "DisplayOutputType",
    "CameraStreamType",
    "ScenarioType",
    "BindingType",
    "TransitionConfigType",
    "MotionPresetType",
    "BluetoothConfigType",
    "WallpaperConfigType",
]
