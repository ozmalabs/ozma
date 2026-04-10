# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""GraphQL types package."""

from .nodes import (
    CameraStreamType as CameraStreamType,
    DisplayOutputType as DisplayOutputType,
    NodeInfoType as NodeInfoType,
    NodeType as NodeType,
)
from .scenarios import (
    BindingType as BindingType,
    ScenarioType as ScenarioType,
    TransitionConfigType as TransitionConfigType,
)

__all__ = [
    "CameraStreamType",
    "DisplayOutputType",
    "NodeInfoType",
    "NodeType",
    "BindingType",
    "ScenarioType",
    "TransitionConfigType",
]
