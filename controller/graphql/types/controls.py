# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GraphQL types for control surfaces and bindings.
"""

import logging
from typing import TYPE_CHECKING

from strawberry import type as graphql_type
from strawberry.types import Info

if TYPE_CHECKING:
    from state import AppState

log = logging.getLogger("ozma.graphql.controls")


@graphql_type
class Binding:
    """
    Represents a binding between a control and an Ozma action.

    Fields:
        action: Action name (e.g., "scenario.activate", "scenario.next")
        target: Target of the action (scenario ID, node name, etc.)
        value: Optional value for the action (e.g., +1/-1 for next scenario)
    """
    action: str
    target: str
    value: str | None


@graphql_type
class ControlBinding:
    """
    Represents a binding for a control on a control surface.

    Fields:
        action: Action name (e.g., "scenario.activate", "scenario.next")
        target: Target of the action (scenario ID, node name, etc.)
        value: Optional value for the action
    """
    action: str
    target: str
    value: str | None


@graphql_type
class Control:
    """
    Represents a named control on a control surface.

    Fields:
        name: Control name
        value: Current control value
        binding: Optional binding to an ozma action
        lockout: Whether this control is locked out (during physical interaction)
    """
    name: str
    value: str | None
    binding: ControlBinding | None
    lockout: bool


@graphql_type
class DisplayBinding:
    """
    Represents a binding for a display element on a control surface.

    Fields:
        binding: What to display (e.g., "@active.name", "@active.color")
    """
    binding: str


@graphql_type
class Display:
    """
    Represents a named display element on a control surface.

    Fields:
        name: Display name
        value: Current display value
        binding: Optional binding to ozma data
    """
    name: str
    value: str
    binding: str


@graphql_type
class ControlSurface:
    """
    Represents a control surface (MIDI, gamepad, Stream Deck, etc.).

    Fields:
        id: Unique identifier for the surface
        name: Human-readable name
        controls: Named controls on this surface
        displays: Named display elements on this surface
        active: Whether the surface is currently active
        surface_type: Type of control surface (midi, gamepad, streamdeck, etc.)
    """
    id: str
    name: str
    controls: list[Control]
    displays: list[Display]
    active: bool
    surface_type: str


async def resolve_control_surface(
    info: Info,
    surface_id: str,
) -> ControlSurface | None:
    """
    Get a specific control surface by ID.

    Args:
        surface_id: ID of the control surface

    Returns:
        ControlSurface: Surface details, or None if not found

    Raises:
        ValueError: If surface_id is empty
    """
    if not surface_id or not surface_id.strip():
        raise ValueError("surface_id cannot be empty")

    state: AppState = info.context["state"]

    # Get controls manager from state
    controls_mgr = getattr(state, 'controls', None)
    if not controls_mgr:
        return None

    surface = controls_mgr._surfaces.get(surface_id)
    if not surface:
        return None

    # Build controls list
    controls_list = []
    for name, control in surface.controls.items():
        binding = None
        if control.binding:
            binding = ControlBinding(
                action=control.binding.action,
                target=control.binding.target or "",
                value=str(control.binding.value) if control.binding.value is not None else None,
            )

        controls_list.append(Control(
            name=name,
            value=str(control.value) if control.value is not None else None,
            binding=binding,
            lockout=control.lockout,
        ))

    # Build displays list
    displays_list = []
    for name, display in surface.displays.items():
        displays_list.append(Display(
            name=name,
            value=display.value,
            binding=display.binding,
        ))

    return ControlSurface(
        id=surface.id,
        name=surface.id,  # Use ID as name
        controls=controls_list,
        displays=displays_list,
        active=True,  # All registered surfaces are active
        surface_type=type(surface).__name__.lower().replace('surface', ''),
    )


async def resolve_all_control_surfaces(info: Info) -> list[ControlSurface]:
    """
    Get all registered control surfaces.
    
    Returns:
        List[ControlSurface]: All control surfaces
    """
    state: AppState = info.context["state"]
    surfaces: list[ControlSurface] = []
    
    controls_mgr = getattr(state, 'controls', None)
    if not controls_mgr:
        return surfaces
    
    for surface_id, surface in controls_mgr._surfaces.items():
        surf = await resolve_control_surface(info, surface_id)
        if surf:
            surfaces.append(surf)
    
    return surfaces


async def resolve_active_control_surface(info: Info) -> ControlSurface | None:
    """
    Get the active control surface (typically the one with recent activity).
    
    Returns:
        ControlSurface: Active surface, or None if no surfaces
    """
    state: AppState = info.context["state"]
    surfaces = await resolve_all_control_surfaces(info)
    
    if not surfaces:
        return None
    
    # Return first active surface (all are active)
    return surfaces[0]
