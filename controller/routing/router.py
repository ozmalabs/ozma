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

from .formats import FormatSet, negotiate_format, NegotiationFailure
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
    def aggregate_loss(self) -> float:
        """
        End-to-end packet loss rate.

        P(packet lost) = 1 - P(packet survives all hops)
                       = 1 - product(1 - loss_i)
        """
        survival = 1.0
        for link in self.links:
            if link.state and link.state.loss:
                survival *= (1.0 - link.state.loss.rate)
        return 1.0 - survival

    @property
    def aggregate_jitter_ms(self) -> float:
        """
        End-to-end p99 jitter: conservative sum of per-hop p99 values.

        True statistical combination requires variance data; per-hop p99 sum
        is a safe upper bound used for constraint checking.
        """
        return sum(
            link.state.jitter.p99_ms
            for link in self.links
            if link.state and link.state.jitter
        )

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

    def __init__(self, graph: RoutingGraph | None = None) -> None:
        self._graph = graph

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

        # 7. Loss — checked as aggregate end-to-end probability
        if c.max_loss is not None:
            agg_loss = path.aggregate_loss
            if agg_loss > c.max_loss:
                violations.append(ConstraintViolation(
                    "loss",
                    f"aggregate loss {agg_loss:.4f} > {c.max_loss} "
                    f"({path.hop_count} hops)"))

        # 8. Jitter — checked as aggregate end-to-end (sum of per-hop p99)
        if c.max_jitter_ms is not None:
            agg_jitter = path.aggregate_jitter_ms
            if agg_jitter > c.max_jitter_ms:
                violations.append(ConstraintViolation(
                    "jitter",
                    f"aggregate p99 jitter {agg_jitter:.2f}ms > {c.max_jitter_ms}ms "
                    f"({path.hop_count} hops)"))

        # 9. Format negotiation — collect format_sets from all ports on the path
        #    and verify they have at least one compatible intersection.
        if self._graph is not None:
            format_sets: list[FormatSet] = []
            for link in path.links:
                for ref in (link.source, link.sink):
                    device = self._graph.get_device(ref.device_id)
                    if device is None:
                        continue
                    port = device.get_port(ref.port_id)
                    if port is not None and port.format_set is not None:
                        format_sets.append(port.format_set)
            if format_sets:
                try:
                    negotiate_format(format_sets, stream.media_type)
                except NegotiationFailure as exc:
                    violations.append(ConstraintViolation(
                        "format", str(exc)))

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
        self._checker = ConstraintChecker(graph)

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

    def recommend_devices(
        self,
        intent: Intent,
        source: Device,
        destination: Device,
        top_n: int = 3,
    ) -> list[tuple[StreamIntent, list[Pipeline]]]:
        """
        Convenience wrapper: resolve Device → PortRef pairs and call recommend().

        For each StreamIntent's media type, picks the first source-direction port
        on *source* and first sink-direction port on *destination* that carry that
        media type, then merges the per-stream results.

        Streams with no matching port on either device are returned with an empty
        pipeline list (same semantics as an infeasible required stream).
        """
        results: list[tuple[StreamIntent, list[Pipeline]]] = []
        for stream in intent.streams:
            mt = stream.media_type
            src_port = next(
                (p for p in source.ports
                 if p.direction == PortDirection.source and p.media_type == mt),
                None,
            )
            dst_port = next(
                (p for p in destination.ports
                 if p.direction == PortDirection.sink and p.media_type == mt),
                None,
            )
            if src_port is None or dst_port is None:
                results.append((stream, []))
                continue
            src_ref = PortRef(device_id=source.id, port_id=src_port.id)
            dst_ref = PortRef(device_id=destination.id, port_id=dst_port.id)
            # Enumerate paths for just this stream
            max_hops = stream.constraints.max_hops or 8
            paths = _enumerate_paths(
                self._graph, src_ref, dst_ref, mt, max_hops=max_hops,
            )
            feasible = [
                p for p in paths if not self._checker.check(p, stream)
            ]
            prefs = stream.preferences
            for p in feasible:
                p.cost = _compute_cost(p, prefs)
            feasible.sort(key=lambda p: p.cost)
            pipelines = [
                self._build_pipeline(intent, stream, p, src_ref, dst_ref)
                for p in feasible[:top_n]
            ]
            results.append((stream, pipelines))
        return results

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
