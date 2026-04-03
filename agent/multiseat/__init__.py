# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Multi-Seat — turn one PC into N independent ozma nodes.

Each physical display + input group becomes a separate seat that registers
with the controller as an independent node. The controller sees N machines
and routes HID, audio, and video to each independently.
"""

from .encoder_allocator import EncoderAllocator, EncoderHints, EncoderSession
from .game_launcher import GameLauncher, GameInfo
from .gpu_inventory import GPUInventory, GPUInfo, EncoderInfo
from .hotplug import HotplugMonitor, SeatPersistence
from .seat import Seat
from .seat_manager import SeatManager
from .seat_profiles import SeatProfile, PROFILES

__all__ = [
    "EncoderAllocator", "EncoderHints", "EncoderSession",
    "GameLauncher", "GameInfo",
    "GPUInventory", "GPUInfo", "EncoderInfo",
    "HotplugMonitor", "SeatPersistence",
    "Seat", "SeatManager", "SeatProfile", "PROFILES",
]
