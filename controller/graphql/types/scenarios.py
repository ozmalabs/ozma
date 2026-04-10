# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for Scenario-related data structures.
"""

from typing import Optional

import strawberry

from controller.scenarios import Scenario, TransitionConfig


@strawberry.type
class TransitionConfigType:
    """Configuration for scenario transition effects."""

    style: str
    duration_ms: int

    @classmethod
    def from_config(cls, config: TransitionConfig) -> "TransitionConfigType":
        """Create from TransitionConfig dataclass."""
        return cls(
            style=config.style,
            duration_ms=config.duration_ms,
        )


@strawberry.type
class MotionPresetType:
    """Motion device preset configuration."""

    device_id: str
    axis: str
    position: float


@strawberry.type
class BluetoothConfigType:
    """Bluetooth device configuration for a scenario."""

    connect: list[str]
    disconnect: list[str]


@strawberry.type
class WallpaperConfigType:
    """Wallpaper configuration for a scenario."""

    mode: str
    color: Optional[str] = None
    image: Optional[str] = None
    url: Optional[str] = None


@strawberry.type
class BindingType:
    """Represents a binding between a node and a scenario context."""

    node_id: Optional[str] = None
    scenario_id: str

    @classmethod
    def from_scenario(cls, scenario: Scenario) -> "BindingType":
        """Create a BindingType from a Scenario dataclass."""
        return cls(
            node_id=scenario.node_id,
            scenario_id=scenario.id,
        )


@strawberry.type
class ScenarioType:
    """A named configuration that binds a compute node to a logical context."""

    id: str
    name: str
    node_id: Optional[str]
    color: str
    transition_in: TransitionConfigType
    motion: list[MotionPresetType]
    bluetooth: list[BluetoothConfigType]
    capture_source: Optional[str]
    capture_sources: list[str]
    wallpaper: Optional[WallpaperConfigType]

    @classmethod
    def from_scenario(cls, scenario: Scenario) -> "ScenarioType":
        """Create a ScenarioType from a Scenario dataclass."""
        return cls(
            id=scenario.id,
            name=scenario.name,
            node_id=scenario.node_id,
            color=scenario.color,
            transition_in=TransitionConfigType.from_config(scenario.transition_in),
            motion=[
                MotionPresetType(
                    device_id=k,
                    axis=v.get("axis", ""),
                    position=v.get("position", 0.0),
                )
                for k, v in (scenario.motion or {}).items()
            ]
            if scenario.motion
            else [],
            bluetooth=[
                BluetoothConfigType(
                    connect=bt.get("connect", []),
                    disconnect=bt.get("disconnect", []),
                )
                for bt in (scenario.bluetooth or [])
            ]
            if scenario.bluetooth
            else [],
            capture_source=scenario.capture_source,
            capture_sources=scenario.capture_sources or [],
            wallpaper=(
                WallpaperConfigType(
                    mode=scenario.wallpaper.get("mode", ""),
                    color=scenario.wallpaper.get("color"),
                    image=scenario.wallpaper.get("image"),
                    url=scenario.wallpaper.get("url"),
                )
                if scenario.wallpaper
                else None
            ),
        )
