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


