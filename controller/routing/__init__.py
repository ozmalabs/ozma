# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma routing graph — Phase 1 + Phase 2.

Phase 1: observational graph model (device/port/link).
Phase 2: intent system, bindings, pipeline primitives, routing engine.
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
from .intent import (
    Constraints,
    Preferences,
    DegradationPolicy,
    StreamIntent,
    Intent,
    BUILTIN_INTENTS,
    compose_intents,
    EncryptionRequirement,
    VideoStrategy,
    AudioStrategy,
    HidStrategy,
)
from .binding import (
    BindingCondition,
    BindingScope,
    RevertPolicy,
    IntentBinding,
    BindingRegistry,
    ConditionOp,
    ConditionMode,
    ConditionSource,
    RevertMode,
    StateResolver,
    DictStateResolver,
)
from .pipeline import (
    Pipeline,
    PipelineHop,
    PipelineMetrics,
    PipelineState,
    WarmthPolicy,
    WarmCost,
    FormatRef,
    LinkRef,
    ConversionRef,
)
from .router import Router

__all__ = [
    # Phase 1 — graph model
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
    # Phase 2 — intent system
    "Constraints",
    "Preferences",
    "DegradationPolicy",
    "StreamIntent",
    "Intent",
    "BUILTIN_INTENTS",
    "compose_intents",
    "EncryptionRequirement",
    "VideoStrategy",
    "AudioStrategy",
    "HidStrategy",
    # Phase 2 — bindings
    "BindingCondition",
    "BindingScope",
    "RevertPolicy",
    "IntentBinding",
    "BindingRegistry",
    "ConditionOp",
    "ConditionMode",
    "ConditionSource",
    "RevertMode",
    "StateResolver",
    "DictStateResolver",
    # Phase 2 — pipeline
    "Pipeline",
    "PipelineHop",
    "PipelineMetrics",
    "PipelineState",
    "WarmthPolicy",
    "WarmCost",
    "FormatRef",
    "LinkRef",
    "ConversionRef",
    # Phase 2 — router
    "Router",
]
