# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Resolvers for control surface-related GraphQL queries.
"""

from typing import Optional, Any

import strawberry
from strawberry.types import Info

from controller.controls import ControlSurface


@strawberry.type
class ControlBindingType:
    """
    GraphQL type for a control binding.

    Maps a physical control to an ozma action.
    """

    action: str
    target: str | None
    value: Any


@strawberry.type
class ControlType:
    """
    GraphQL type for a control on a control surface.

    Represents a physical control (button, fader, encoder) that can be
    bound to an ozma action.
    """

    name: str
    value: Any
    binding: ControlBindingType | None
    lockout: bool


@strawberry.type
class DisplayControlType:
    """
    GraphQL type for a display control on a surface.

    Represents an LCD display element (like X-Touch scribble strips or
    Stream Deck key labels).
    """

    name: str
    value: str
    binding: str


@strawberry.type
class ControlSurfaceType:
    """
    GraphQL type for a control surface.

    Represents a physical device like a MIDI controller, Stream Deck,
    or gamepad that can be used to control the ozma system.
    """

    id: str
    type: str
    device: str | None
    controls: list[ControlType]
    displays: list[DisplayControlType]


@strawberry.type
class QueryControls:
    """
    Query resolvers for control surfaces.
    """

    @strawberry.field
    def control_surfaces(self, info: Info) -> list[ControlSurfaceType]:
        """Query all connected control surfaces."""
        return _resolve_control_surfaces(info)

    @strawberry.field
    def control_surface(self, info: Info, id: str) -> ControlSurfaceType | None:
        """Query a single control surface by ID."""
        surfaces = _resolve_control_surfaces(info)
        for surface in surfaces:
            if surface.id == id:
                return surface
        return None


def _resolve_control_surfaces(info: Info) -> list[ControlSurfaceType]:
    """
    Resolve all connected control surfaces.

    Args:
        info: Strawberry info context containing controls manager

    Returns:
        List of ControlSurfaceType objects
    """
    controls_mgr = info.context.get("controls")

    if not controls_mgr:
        return []

    surfaces = controls_mgr.list_surfaces()
    result = []

    for surface_data in surfaces:
        surface_id = surface_data.get("id", "")
        # Determine surface type from id prefix
        surface_type = surface_id.split("-")[0] if "-" in surface_id else "unknown"

        controls_list = []
        controls_data = surface_data.get("controls", {})
        for name, ctrl_data in controls_data.items():
            binding_data = ctrl_data.get("binding")
            binding = None
            if binding_data:
                # Parse the binding from dict format
                if isinstance(binding_data, dict):
                    binding = ControlBindingType(
                        action=binding_data.get("action", ""),
                        target=binding_data.get("target"),
                        value=binding_data.get("value"),
                    )
                elif isinstance(binding_data, str):
                    # Fallback for string format
                    binding = ControlBindingType(
                        action=binding_data,
                        target=None,
                        value=None,
                    )
            controls_list.append(ControlType(
                name=name,
                value=ctrl_data.get("value"),
                binding=binding,
                lockout=ctrl_data.get("lockout", False),
            ))

        displays_list = []
        displays_data = surface_data.get("displays", {})
        for name, disp_data in displays_data.items():
            displays_list.append(DisplayControlType(
                name=name,
                value=disp_data.get("value", ""),
                binding=disp_data.get("binding", ""),
            ))

        result.append(ControlSurfaceType(
            id=surface_id,
            type=surface_type,
            device=surface_data.get("device"),
            controls=controls_list,
            displays=displays_list,
        ))

    return result
