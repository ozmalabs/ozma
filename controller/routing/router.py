# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Routing engine — intent + graph → ranked pipeline candidates.

Implements the constraint satisfaction and cost model from docs/routing/routing.md.
Phase 2: intent-driven pipeline recommendation. No activation logic here — the
controller uses the ranked list and activates the best candidate.

Algorithm:
  1. For each StreamIntent in the active Intent, enumerate all source→destination
     paths through the RoutingGraph that match the media type.
  2. Apply 12-point constraint satisfaction filter (hard rejects).
  3. Rank surviving candidates by cost function (weighted preferences).
  4. Return top-N ranked Pipelines per stream.
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass, field

from .graph import RoutingGraph
from .intent import Intent, StreamIntent, Preferences
from .model import (
    Device, Link, MediaType, PortRef, PortDirection,
    InfoQuality, LinkStatus,
)
from .pipeline import (
    Pipeline, PipelineHop, PipelineMetrics, PipelineState,
    FormatRef, LinkRef, WarmthPolicy,
)

log = logging.getLogger("ozma.routing.router")


# ── Constraint violation ──────────────────────────────────────────────────────

@dataclass
class ConstraintViolation:
    check: str
    detail: str

    def __str__(self) -> str:
        return f"{self.check}: {self.detail}"


# ── Candidate path ────────────────────────────────────────────────────────────

@dataclass
class CandidatePath:
    """A raw path through the graph before being turned into a Pipeline."""
    links: list[Link] = field(default_factory=list)
    violations: list[ConstraintViolation] = field(default_factory=list)
    cost: float = 0.0

    @property
    def feasible(self) -> bool:
        return len(self.violations) == 0

    @property
    def hop_count(self) -> int:
        return len(self.links)

    @property
    def conversion_count(self) -> int:
        # Phase 2: we don't model converters yet — each distinct codec change is 1
        return 0

    @property
    def total_latency_ms(self) -> float:
        total = 0.0
        for link in self.links:
            ls = link.state
            if ls and ls.latency:
                total += ls.latency.typical_ms
        return total

    @property
    def bottleneck_bandwidth_bps(self) -> int:
        bws = [
            link.state.bandwidth.available_bps
            for link in self.links
            if link.state and link.state.bandwidth
        ]
        return min(bws) if bws else 0

    @property
    def weakest_quality(self) -> InfoQuality:
        worst = InfoQuality.user
        for link in self.links:
            if link.state:
                if link.state.bandwidth and link.state.bandwidth.quality < worst:
                    worst = link.state.bandwidth.quality
                if link.state.latency and link.state.latency.quality < worst:
                    worst = link.state.latency.quality
        return worst


# ── Constraint checker ────────────────────────────────────────────────────────

class ConstraintChecker:
    """
    12-point constraint satisfaction check per docs/routing/routing.md.

    All checks are hard filters — any failure disqualifies the path.
    """

    def check(
        self,
        path: CandidatePath,
        stream: StreamIntent,
    ) -> list[ConstraintViolation]:
        violations: list[ConstraintViolation] = []
        c = stream.constraints

        # 1. Latency
        if c.max_latency_ms is not None:
            lat = path.total_latency_ms
            if lat > c.max_latency_ms:
                violations.append(ConstraintViolation(
                    "latency", f"{lat:.1f}ms > {c.max_latency_ms}ms"))

        # 2. Activation time (Phase 2: no activation data yet — skip if unknown)
        # (left as pass-through; data arrives in Phase 5)

        # 3. Bandwidth
        if c.min_bandwidth_bps is not None:
            bw = path.bottleneck_bandwidth_bps
            if bw > 0 and bw < c.min_bandwidth_bps:
                violations.append(ConstraintViolation(
                    "bandwidth",
                    f"bottleneck {bw}bps < required {c.min_bandwidth_bps}bps"))

        # 4. Device capacity (Phase 2: placeholder — no resource data yet)

        # 5. Resource budget (Phase 2: placeholder)

        # 6. Power budget (Phase 2: placeholder)

        # 7. Loss
        if c.max_loss is not None:
            for link in path.links:
                if link.state and link.state.loss:
                    if link.state.loss.rate > c.max_loss:
                        violations.append(ConstraintViolation(
                            "loss",
                            f"link {link.id} loss {link.state.loss.rate:.4f} > {c.max_loss}"))

        # 8. Jitter
        if c.max_jitter_ms is not None:
            for link in path.links:
                if link.state and link.state.jitter:
                    if link.state.jitter.p99_ms > c.max_jitter_ms:
                        violations.append(ConstraintViolation(
                            "jitter",
                            f"link {link.id} p99 jitter {link.state.jitter.p99_ms}ms > {c.max_jitter_ms}ms"))

        # 9. Format (Phase 2: we don't negotiate formats yet — skip)

        # 10. Hops
        if c.max_hops is not None and path.hop_count > c.max_hops:
            violations.append(ConstraintViolation(
                "hops", f"{path.hop_count} > {c.max_hops}"))

        # 11. Conversions
        if c.max_conversions is not None and path.conversion_count > c.max_conversions:
            violations.append(ConstraintViolation(
                "conversions", f"{path.conversion_count} > {c.max_conversions}"))

        # 12. Encryption (Phase 2: transport not modelled yet — skip unless required)
        # If encryption is "required" but transport provides none, we'd fail here.
        # Deferred to transport layer (Phase 4).

        return violations


# ── Cost function ─────────────────────────────────────────────────────────────

# Default weight set — tuned from docs/routing/routing.md cost model.
DEFAULT_WEIGHTS = {
    "latency":      1.0,
    "hops":         0.5,
    "conversions":  2.0,
    "bandwidth":    0.3,
    "quality_loss": 1.5,
    "uncertainty":  0.8,
    "pressure":     1.2,
}

# Trust factor by InfoQuality level (1.0 = fully trusted, 0.0 = unknown)
_TRUST_FACTOR: dict[InfoQuality, float] = {
    InfoQuality.user:      1.0,
    InfoQuality.measured:  0.9,
    InfoQuality.inferred:  0.7,
    InfoQuality.reported:  0.5,
    InfoQuality.commanded: 0.4,
    InfoQuality.spec:      0.3,
    InfoQuality.assumed:   0.1,
}


def _compute_cost(path: CandidatePath, prefs: Preferences) -> float:
    """
    Compute routing cost for a candidate path given the active preferences.

    Lower cost = better candidate.
    """
    w = dict(DEFAULT_WEIGHTS)

    # Adjust weights based on preferences
    if prefs.prefer_lower_latency:
        w["latency"] *= 2.0
    if prefs.prefer_fewer_hops:
        w["hops"] *= 2.0
    if prefs.prefer_higher_quality:
        w["quality_loss"] *= 2.0
        w["uncertainty"] *= 1.5
    if prefs.prefer_lossless:
        w["conversions"] *= 3.0

    # Normalise latency against target if given, else against 1000ms
    latency_norm = path.total_latency_ms / (prefs.target_latency_ms or 1000.0)

    # Bandwidth term: how far below the target are we? (0 = fine)
    bw_term = 0.0
    if path.bottleneck_bandwidth_bps > 0:
        target_bw = 10_000_000  # 10 Mbps default reference
        bw_term = max(0.0, 1.0 - path.bottleneck_bandwidth_bps / target_bw)

    # Quality loss = 1 - trust_factor of weakest link
    quality_loss = 1.0 - _TRUST_FACTOR.get(path.weakest_quality, 0.1)

    # Uncertainty = same as quality_loss in Phase 2
    uncertainty = quality_loss

    cost = (
        w["latency"] * latency_norm
        + w["hops"] * path.hop_count
        + w["conversions"] * path.conversion_count
        + w["bandwidth"] * bw_term
        + w["quality_loss"] * quality_loss
        + w["uncertainty"] * uncertainty
        # pressure term: Phase 5 (no resource data yet)
    )
    return cost


# ── Path enumeration (DFS) ────────────────────────────────────────────────────

def _enumerate_paths(
    graph: RoutingGraph,
    source: PortRef,
    destination: PortRef,
    media_type: MediaType,
    max_hops: int = 8,
) -> list[CandidatePath]:
    """
    Enumerate all acyclic paths from source to destination for the given
    media type, up to max_hops links deep.

    Returns raw CandidatePaths (no constraint checking yet).
    """
    results: list[CandidatePath] = []
    _dfs(graph, source, destination, media_type, max_hops, [], set(), results)
    return results


def _dfs(
    graph: RoutingGraph,
    current: PortRef,
    destination: PortRef,
    media_type: MediaType,
    remaining_hops: int,
    path: list[Link],
    visited_devices: set[str],
    results: list[CandidatePath],
) -> None:
    if remaining_hops < 0:
        return

    # Find all links from current port whose media type matches
    for link in graph.links_from(current):
        if link.state and link.state.status == LinkStatus.failed:
            continue
        # Check media type compatibility: link format or port capability
        sink_device_id = link.sink.device_id
        if sink_device_id in visited_devices:
            continue  # cycle guard

        new_path = path + [link]

        if link.sink == destination:
            results.append(CandidatePath(links=new_path))
            continue

        # Recurse: find links that originate from devices reachable via link.sink.
        # We already consumed one hop for this link, so only recurse if there
        # are more hops available beyond the current one.
        if remaining_hops > 1:
            # Find source ports on the sink device that carry this media type
            sink_device = graph.get_device(sink_device_id)
            if sink_device is None:
                continue
            for port in sink_device.ports:
                if port.direction != PortDirection.source:
                    continue
                if port.media_type != media_type:
                    continue
                next_ref = PortRef(device_id=sink_device_id, port_id=port.id)
                _dfs(
                    graph, next_ref, destination, media_type,
                    remaining_hops - 1, new_path,
                    visited_devices | {sink_device_id}, results,
                )


# ── Router ────────────────────────────────────────────────────────────────────

class Router:
    """
    Routing engine: intent + RoutingGraph → ranked Pipeline candidates.

    Usage:
        router = Router(graph)
        results = router.recommend(intent, source, destination, top_n=3)
        # results: list of (StreamIntent, list[Pipeline]) — one entry per stream
    """

    def __init__(self, graph: RoutingGraph) -> None:
        self._graph = graph
        self._checker = ConstraintChecker()

    def recommend(
        self,
        intent: Intent,
        source: PortRef,
        destination: PortRef,
        top_n: int = 3,
    ) -> list[tuple[StreamIntent, list[Pipeline]]]:
        """
        For each StreamIntent in *intent*, find and rank candidate Pipelines
        from *source* to *destination*.

        Returns a list of (stream_intent, ranked_pipelines) — one entry per
        stream in the intent. Required streams with no feasible path are
        included with an empty list (the caller must handle this as a failure).
        """
        results: list[tuple[StreamIntent, list[Pipeline]]] = []

        for stream in intent.streams:
            max_hops = stream.constraints.max_hops or 8
            paths = _enumerate_paths(
                self._graph, source, destination,
                stream.media_type, max_hops=max_hops,
            )

            # Apply constraint satisfaction
            feasible: list[CandidatePath] = []
            for path in paths:
                violations = self._checker.check(path, stream)
                if not violations:
                    feasible.append(path)
                else:
                    log.debug(
                        "Path rejected for %s stream (%d hops): %s",
                        stream.media_type.value,
                        path.hop_count,
                        "; ".join(str(v) for v in violations),
                    )

            # Rank by cost
            prefs = stream.preferences
            for path in feasible:
                path.cost = _compute_cost(path, prefs)
            feasible.sort(key=lambda p: p.cost)

            # Build Pipeline objects for top-N
            pipelines = [
                self._build_pipeline(intent, stream, path, source, destination)
                for path in feasible[:top_n]
            ]

            results.append((stream, pipelines))

        return results

    def _build_pipeline(
        self,
        intent: Intent,
        stream: StreamIntent,
        path: CandidatePath,
        source: PortRef,
        destination: PortRef,
    ) -> Pipeline:
        hops = [
            PipelineHop(
                link=LinkRef(link_id=link.id),
                latency_contribution_ms=(
                    link.state.latency.typical_ms
                    if link.state and link.state.latency else 0.0
                ),
                current_state=(
                    link.state.status
                    if link.state else LinkStatus.unknown
                ),
            )
            for link in path.links
        ]

        pipeline = Pipeline(
            id=str(uuid.uuid4()),
            intent=intent,
            source=source,
            destination=destination,
            hops=hops,
            state=PipelineState.standby,
        )
        pipeline.recompute_metrics()
        # Override with more accurate metrics from path
        pipeline.aggregate.total_latency_ms = path.total_latency_ms
        pipeline.aggregate.bottleneck_bandwidth_bps = path.bottleneck_bandwidth_bps
        pipeline.aggregate.total_hops = path.hop_count
        pipeline.aggregate.total_conversions = path.conversion_count
        pipeline.aggregate.weakest_quality = path.weakest_quality

        return pipeline

    def check_feasibility(
        self,
        intent: Intent,
        source: PortRef,
        destination: PortRef,
    ) -> dict[MediaType, bool]:
        """
        Quick feasibility check: can each required stream be routed?

        Returns {media_type: feasible} for all streams in the intent.
        """
        results: dict[MediaType, bool] = {}
        for stream in intent.streams:
            if not stream.required:
                results[stream.media_type] = True
                continue
            max_hops = stream.constraints.max_hops or 8
            paths = _enumerate_paths(
                self._graph, source, destination,
                stream.media_type, max_hops=max_hops,
            )
            feasible = any(
                not self._checker.check(p, stream) for p in paths
            )
            results[stream.media_type] = feasible
        return results
