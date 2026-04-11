# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""GraphQL types for Scenario representation."""

from typing import Any

import strawberry

from ..scenarios import Scenario, TransitionConfig


@strawberry.type
class TransitionConfigType:
    """Configuration for scenario transition effects."""

    style: str
    duration_ms: int

    @classmethod
    def from_transition(cls, config: TransitionConfig) -> "TransitionConfigType":
        """Create a TransitionConfigType from a TransitionConfig instance."""
        return cls(
            style=config.style,
            duration_ms=config.duration_ms,
        )


@strawberry.type
class ScenarioType:
    """A named configuration that binds a compute node to a logical context."""

    id: str
    name: str
    node_id: str | None
    color: str
    transition_in: TransitionConfigType
    motion: dict | None
    bluetooth: dict | None
    capture_source: str | None
    capture_sources: list[str] | None
    wallpaper: dict | None

    @classmethod
    def from_scenario(cls, scenario: Scenario) -> "ScenarioType":
        """Create a ScenarioType from a Scenario instance."""
        return cls(
            id=scenario.id,
            name=scenario.name,
            node_id=scenario.node_id,
            color=scenario.color,
            transition_in=TransitionConfigType.from_transition(scenario.transition_in),
            motion=scenario.motion,
            bluetooth=scenario.bluetooth,
            capture_source=scenario.capture_source,
            capture_sources=scenario.capture_sources,
            wallpaper=scenario.wallpaper,
        )


@strawberry.type
class BindingType:
    """Represents a binding between a scenario and a node."""

    id: str
    scenario_id: str
    node_id: str | None
    active: bool

    @classmethod
    def from_scenario(cls, scenario: Scenario, active: bool = False) -> "BindingType":
        """Create a BindingType from a Scenario instance."""
        return cls(
            id=scenario.id,
            scenario_id=scenario.id,
            node_id=scenario.node_id,
            active=active,
        )
