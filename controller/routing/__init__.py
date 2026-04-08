# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma routing graph — Phase 1: observational graph model.

Builds the device/port/link graph from existing discovery data. No routing
decisions are made yet — the graph is read-only and observational.
"""

from .model import (
    InfoQuality,
    Location,
    PhysicalLocation,
    HardwareIdentity,
    BandwidthSpec,
    LatencySpec,
    JitterSpec,
    LossSpec,
    ActivationTimeSpec,
    PortState,
    LinkState,
    PortRef,
    Device,
    Port,
    Link,
    DeviceType,
    MediaType,
    PortDirection,
    LinkStatus,
)
from .graph import RoutingGraph
from .builder import GraphBuilder

__all__ = [
    "InfoQuality",
    "Location",
    "PhysicalLocation",
    "HardwareIdentity",
    "BandwidthSpec",
    "LatencySpec",
    "JitterSpec",
    "LossSpec",
    "ActivationTimeSpec",
    "PortState",
    "LinkState",
    "PortRef",
    "Device",
    "Port",
    "Link",
    "DeviceType",
    "MediaType",
    "PortDirection",
    "LinkStatus",
    "RoutingGraph",
    "GraphBuilder",
]
