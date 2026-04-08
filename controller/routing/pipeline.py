# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Pipeline primitive for the Ozma routing graph.

Implements Pipeline, PipelineHop, PipelineMetrics, WarmthPolicy and WarmCost
from docs/routing/graph-primitives.md §Pipeline.

A Pipeline is a fully-specified, end-to-end path through the graph for a single
intent. Multiple candidate pipelines may exist; the router ranks them and the
controller activates the best one.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from .model import (
    ActivationTimeSpec,
    InfoQuality,
    LinkStatus,
    PortRef,
)
from .intent import Intent


# ── Pipeline state ────────────────────────────────────────────────────────────

class PipelineState(str, Enum):
    active = "active"
    warm = "warm"
    standby = "standby"
    failed = "failed"


# ── Format reference ──────────────────────────────────────────────────────────

@dataclass
class FormatRef:
    """
    Lightweight reference to a negotiated format on a hop.

    Full format objects live in the format system; here we store enough to
    describe the hop without depending on the full Format hierarchy.
    """
    media_type: str
    codec: str | None = None
    container: str | None = None
    # video
    width: int | None = None
    height: int | None = None
    framerate: float | None = None
    # audio
    sample_rate: int | None = None
    channels: int | None = None
    bit_depth: int | None = None
    # generic
    lossy: bool = False
    description: str = ""

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "media_type": self.media_type,
            "lossy": self.lossy,
        }
        for attr in ("codec", "container", "width", "height", "framerate",
                     "sample_rate", "channels", "bit_depth", "description"):
            v = getattr(self, attr)
            if v is not None and v != "":
                d[attr] = v
        return d


# ── ConversionRef ─────────────────────────────────────────────────────────────

@dataclass
class ConversionRef:
    """Reference to a format conversion applied at a hop."""
    device_id: str
    description: str = ""

    def to_dict(self) -> dict:
        return {"device_id": self.device_id, "description": self.description}


# ── LinkRef ───────────────────────────────────────────────────────────────────

@dataclass
class LinkRef:
    """Lightweight reference to a Link in the RoutingGraph."""
    link_id: str

    def to_dict(self) -> dict:
        return {"link_id": self.link_id}


# ── PipelineHop ───────────────────────────────────────────────────────────────

@dataclass
class PipelineHop:
    """
    One hop in a Pipeline — a single link traversal with optional format conversion.
    """
    link: LinkRef
    input_format: FormatRef | None = None
    output_format: FormatRef | None = None
    conversion: ConversionRef | None = None
    latency_contribution_ms: float = 0.0
    activation_time: ActivationTimeSpec | None = None
    current_state: LinkStatus = LinkStatus.unknown

    @property
    def has_conversion(self) -> bool:
        return self.conversion is not None

    def to_dict(self) -> dict:
        return {
            "link": self.link.to_dict(),
            "input_format": self.input_format.to_dict() if self.input_format else None,
            "output_format": self.output_format.to_dict() if self.output_format else None,
            "conversion": self.conversion.to_dict() if self.conversion else None,
            "latency_contribution_ms": self.latency_contribution_ms,
            "activation_time": self.activation_time.to_dict() if self.activation_time else None,
            "current_state": self.current_state.value,
        }


# ── PipelineMetrics ───────────────────────────────────────────────────────────

@dataclass
class PipelineMetrics:
    """Aggregate end-to-end metrics for a Pipeline."""
    total_latency_ms: float = 0.0
    bottleneck_bandwidth_bps: int = 0
    total_conversions: int = 0
    total_hops: int = 0
    end_to_end_jitter_ms: float = 0.0
    end_to_end_loss: float = 0.0
    weakest_quality: InfoQuality = InfoQuality.assumed
    activation_time_ms: float = 0.0        # cold start
    warm_activation_time_ms: float = 0.0   # from warm state
    cold_activation_time_ms: float = 0.0   # from cold (== activation_time_ms)

    def to_dict(self) -> dict:
        return {
            "total_latency_ms": self.total_latency_ms,
            "bottleneck_bandwidth_bps": self.bottleneck_bandwidth_bps,
            "total_conversions": self.total_conversions,
            "total_hops": self.total_hops,
            "end_to_end_jitter_ms": self.end_to_end_jitter_ms,
            "end_to_end_loss": self.end_to_end_loss,
            "weakest_quality": self.weakest_quality.value,
            "activation_time_ms": self.activation_time_ms,
            "warm_activation_time_ms": self.warm_activation_time_ms,
            "cold_activation_time_ms": self.cold_activation_time_ms,
        }

    @classmethod
    def from_hops(cls, hops: list[PipelineHop]) -> "PipelineMetrics":
        """Compute aggregate metrics from a list of PipelineHops."""
        if not hops:
            return cls()

        total_latency = sum(h.latency_contribution_ms for h in hops)
        total_conversions = sum(1 for h in hops if h.has_conversion)
        total_hops = len(hops)

        # Activation time: sum of all cold-start times (worst case)
        cold_ms = sum(
            h.activation_time.cold_to_warm_ms + h.activation_time.warm_to_active_ms
            for h in hops
            if h.activation_time is not None
        )
        warm_ms = sum(
            h.activation_time.warm_to_active_ms
            for h in hops
            if h.activation_time is not None
        )

        # Weakest quality: lowest trust level across all hops
        weakest = InfoQuality.user
        for h in hops:
            if h.activation_time is not None and h.activation_time.quality < weakest:
                weakest = h.activation_time.quality

        return cls(
            total_latency_ms=total_latency,
            total_conversions=total_conversions,
            total_hops=total_hops,
            weakest_quality=weakest,
            activation_time_ms=cold_ms,
            warm_activation_time_ms=warm_ms,
            cold_activation_time_ms=cold_ms,
        )


# ── WarmCost ──────────────────────────────────────────────────────────────────

@dataclass
class WarmCost:
    """Resource cost of keeping a pipeline in the warm state."""
    cpu_percent: float = 0.0
    memory_mb: float = 0.0
    bandwidth_bps: int = 0
    gpu_slots: int = 0
    description: str = ""

    def to_dict(self) -> dict:
        return {
            "cpu_percent": self.cpu_percent,
            "memory_mb": self.memory_mb,
            "bandwidth_bps": self.bandwidth_bps,
            "gpu_slots": self.gpu_slots,
            "description": self.description,
        }


# ── WarmthPolicy ─────────────────────────────────────────────────────────────

@dataclass
class WarmthPolicy:
    """Controls whether and how long a pipeline is kept warm."""
    keep_warm: bool = False
    warm_priority: int = 50           # 0–100; higher = keep warm longer under pressure
    max_warm_duration_s: int | None = None   # None = forever
    warm_cost: WarmCost = field(default_factory=WarmCost)

    def to_dict(self) -> dict:
        return {
            "keep_warm": self.keep_warm,
            "warm_priority": self.warm_priority,
            "max_warm_duration_s": self.max_warm_duration_s,
            "warm_cost": self.warm_cost.to_dict(),
        }


# ── Pipeline ──────────────────────────────────────────────────────────────────

@dataclass
class Pipeline:
    """
    A fully-specified, end-to-end path through the RoutingGraph.

    The router produces Pipelines as candidates; the controller activates one
    per active intent. A Pipeline is associated with a single intent and carries
    exactly one media stream end-to-end.
    """
    id: str
    intent: Intent
    source: PortRef
    destination: PortRef
    hops: list[PipelineHop] = field(default_factory=list)
    aggregate: PipelineMetrics = field(default_factory=PipelineMetrics)
    state: PipelineState = PipelineState.standby
    warmth_policy: WarmthPolicy = field(default_factory=WarmthPolicy)

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "intent": self.intent.name,
            "source": self.source.to_dict(),
            "destination": self.destination.to_dict(),
            "hops": [h.to_dict() for h in self.hops],
            "aggregate": self.aggregate.to_dict(),
            "state": self.state.value,
            "warmth_policy": self.warmth_policy.to_dict(),
        }

    def recompute_metrics(self) -> None:
        """Recompute aggregate metrics from current hops list."""
        self.aggregate = PipelineMetrics.from_hops(self.hops)
