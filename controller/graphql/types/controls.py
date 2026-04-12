# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for control surfaces.

Supports MIDI controllers, Stream Decks, gamepads, and hotkeys.
"""

import logging
from dataclasses import field
from typing import Any

import strawberry
from strawberry.types import Info

log = logging.getLogger("ozma.graphql.controls")


@strawberry.type
class ControlBindingType:
    """
    GraphQL type for a control binding.

    Maps a physical control to an ozma action.
    """

    action: str = ""
    target: str | None = None
    value: Any = None


@strawberry.type
class ControlType:
    """
    GraphQL type for a control on a control surface.

    Represents a physical control (button, fader, encoder) that can be
    bound to an ozma action.
    """

    name: str = ""
    value: Any = None
    binding: ControlBindingType | None = None
    lockout: bool = False


@strawberry.type
class DisplayControlType:
    """
    GraphQL type for a display control on a surface.

    Represents an LCD display element (like X-Touch scribble strips or
    Stream Deck key labels).
    """

    name: str = ""
    value: str = ""
    binding: str = ""


@strawberry.type
class ControlSurfaceType:
    """
    GraphQL type for a control surface.

    Represents a physical device like a MIDI controller, Stream Deck,
    or gamepad that can be used to control the ozma system.
    """

    id: str = ""
    type: str = ""
    device: str | None = None
    controls: list[ControlType] = field(default_factory=list)
    displays: list[DisplayControlType] = field(default_factory=list)


async def resolve_control_surfaces(info: Info) -> list[ControlSurfaceType]:
    """
    Resolve all connected control surfaces.

    Args:
        info: Strawberry info context containing app_state and controls manager

    Returns:
        List of ControlSurfaceType objects
    """
    from controller.state import AppState
    from controller.controls import ControlSurface

    app_state: AppState = info.context.get("state")
    controls_mgr = info.context.get("controls")

    if not controls_mgr:
        return []

    surfaces = controls_mgr.list_surfaces()
    result = []

    for surface_data in surfaces:
        # Validate that surface_data is a dict
        if not isinstance(surface_data, dict):
            log.warning("Invalid surface data: expected dict, got %s", type(surface_data).__name__)
            continue

        surface_id = surface_data.get("id", "")
        if not surface_id:
            log.warning("Skipping surface with missing id")
            continue

        # Determine surface type from id prefix (format: "<type>-<name>")
        # Handle various id formats safely
        surface_type = "unknown"
        if "-" in surface_id:
            parts = surface_id.split("-", 1)
            if parts:
                surface_type = parts[0]

        controls_list = []
        controls_data = surface_data.get("controls", {})
        if not isinstance(controls_data, dict):
            log.warning("Invalid controls data for surface %s: expected dict, got %s",
                        surface_id, type(controls_data).__name__)
            controls_data = {}

        for name, ctrl_data in controls_data.items():
            if not isinstance(ctrl_data, dict):
                log.warning("Invalid control data for control %s on surface %s: expected dict, got %s",
                            name, surface_id, type(ctrl_data).__name__)
                continue

            binding_data = ctrl_data.get("binding")
            binding = None
            if binding_data is not None:
                # Parse the binding from dict format
                if isinstance(binding_data, dict):
                    action = binding_data.get("action")
                    if action is not None:
                        binding = ControlBindingType(
                            action=str(action),
                            target=str(binding_data.get("target")) if binding_data.get("target") is not None else None,
                            value=binding_data.get("value"),
                        )
                elif isinstance(binding_data, str):
                    # Fallback for string format
                    binding = ControlBindingType(
                        action=binding_data,
                    )
            controls_list.append(ControlType(
                name=name,
                value=ctrl_data.get("value"),
                binding=binding,
                lockout=bool(ctrl_data.get("lockout", False)),
            ))

        displays_list = []
        displays_data = surface_data.get("displays", {})
        if not isinstance(displays_data, dict):
            log.warning("Invalid displays data for surface %s: expected dict, got %s",
                        surface_id, type(displays_data).__name__)
            displays_data = {}

        for name, disp_data in displays_data.items():
            if not isinstance(disp_data, dict):
                log.warning("Invalid display data for display %s on surface %s: expected dict, got %s",
                            name, surface_id, type(disp_data).__name__)
                continue

            displays_list.append(DisplayControlType(
                name=name,
                value=str(disp_data.get("value", "")),
                binding=str(disp_data.get("binding", "")),
            ))

        result.append(ControlSurfaceType(
            id=surface_id,
            type=surface_type,
            device=str(surface_data.get("device")) if surface_data.get("device") is not None else None,
            controls=controls_list,
            displays=displays_list,
        ))

    return result


@strawberry.type
class QueryControls:
    """
    Query resolvers for control surfaces.
    """

    @strawberry.field
    def control_surfaces(self, info: Info) -> list[ControlSurfaceType]:
        """Query all connected control surfaces."""
        return resolve_control_surfaces(info)

    @strawberry.field
    def control_surface(self, info: Info, id: str) -> ControlSurfaceType | None:
        """Query a single control surface by ID."""
        surfaces = resolve_control_surfaces(info)
        for surface in surfaces:
            if surface.id == id:
                return surface
        return None
