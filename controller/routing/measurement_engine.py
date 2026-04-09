# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Active measurement engine — Phase 5 runtime component.

Runs as a background asyncio task. Periodically probes each routable link in
the RoutingGraph for latency, loss, and jitter, then records the results in
MeasurementStore with InfoQuality.measured.

Design principles:
  - One asyncio task per engine; cancelled on shutdown.
  - Probes are staggered across links so startup doesn't spike the network.
  - Each link is probed at the "network_health" RefreshClass interval, adapted
    for whether the link carries an active pipeline.
  - Probe failures are recorded as errors in DeviceFreshness; consecutive
    failures cause link.state.status → LinkStatus.failed in the graph.
  - Only links with a `target_ip` property are probed; USB/PipeWire links
    without a network target are skipped.
"""

from __future__ import annotations

import asyncio
import logging
import math
import re
import time
from typing import TYPE_CHECKING

from .model import InfoQuality, LinkStatus

if TYPE_CHECKING:
    from .graph import RoutingGraph
    from .measurement import MeasurementStore
    from .monitoring import MonitoringJournal, MetricStore

log = logging.getLogger("ozma.routing.measurement_engine")

# ── Probe configuration ───────────────────────────────────────────────────────

# How many ICMP echo requests to send per probe
_PING_COUNT = 4
# Ping timeout in seconds (per-packet)
_PING_TIMEOUT_S = 2.0
# How many consecutive probe failures before a link is marked failed
_FAILURE_THRESHOLD = 3
# Default probe interval (matches network_health RefreshClass default_interval_s)
_DEFAULT_INTERVAL_S = 7.5
# Minimum interval regardless of adaptive factors
_MIN_INTERVAL_S = 1.0
# Stagger between probing successive links on startup (seconds)
_STAGGER_S = 0.5


# ── Ping result ───────────────────────────────────────────────────────────────

class PingResult:
    """Parsed result from a single ICMP probe."""

    __slots__ = (
        "host", "sent", "received", "loss_rate",
        "min_ms", "avg_ms", "max_ms", "mdev_ms",
    )

    def __init__(
        self,
        host: str,
        sent: int,
        received: int,
        min_ms: float,
        avg_ms: float,
        max_ms: float,
        mdev_ms: float,
    ) -> None:
        self.host = host
        self.sent = sent
        self.received = received
        self.loss_rate = 1.0 - (received / sent) if sent > 0 else 1.0
        self.min_ms = min_ms
        self.avg_ms = avg_ms
        self.max_ms = max_ms
        self.mdev_ms = mdev_ms

    @property
    def p99_jitter_ms(self) -> float:
        """
        Conservative p99 jitter estimate from mdev (mean deviation).

        For a normal distribution, p99 ≈ mean ± 2.33σ. mdev approximates
        σ, so p99 ≈ mean + 2.33 × mdev. We return just the jitter component
        (2.33 × mdev), saturated at 1000ms to cap absurd values.
        """
        return min(2.33 * self.mdev_ms, 1000.0)

    def __repr__(self) -> str:
        return (
            f"PingResult({self.host!r} "
            f"sent={self.sent} recv={self.received} "
            f"loss={self.loss_rate:.1%} "
            f"avg={self.avg_ms:.1f}ms mdev={self.mdev_ms:.1f}ms)"
        )


async def _run_ping(host: str, count: int = _PING_COUNT) -> PingResult | None:
    """
    Run `ping -c {count} -W {timeout} {host}` and parse the output.

    Returns None on subprocess error or unparseable output.
    """
    timeout_int = max(1, math.ceil(_PING_TIMEOUT_S))
    cmd = ["ping", "-c", str(count), "-W", str(timeout_int), host]
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        timeout = _PING_TIMEOUT_S * count + 5.0
        stdout_b, _ = await asyncio.wait_for(proc.communicate(), timeout=timeout)
    except (asyncio.TimeoutError, FileNotFoundError, OSError) as exc:
        log.debug("ping failed for %s: %s", host, exc)
        return None

    stdout = stdout_b.decode(errors="replace")
    return _parse_ping_output(host, stdout, count)


# ping output patterns for Linux and macOS
_STATS_RE = re.compile(
    r"(\d+) packets transmitted,\s*(\d+)\s+(?:packets )?received",
)
_RTT_RE = re.compile(
    r"(?:rtt|round-trip) min/avg/max/(?:mdev|stddev)\s*=\s*"
    r"([\d.]+)/([\d.]+)/([\d.]+)/([\d.]+)",
)


def _parse_ping_output(host: str, stdout: str, sent: int) -> PingResult | None:
    """Parse Linux/macOS ping output into a PingResult."""
    stats_m = _STATS_RE.search(stdout)
    rtt_m = _RTT_RE.search(stdout)

    if not stats_m:
        log.debug("Could not parse ping stats for %s", host)
        return None

    actual_sent = int(stats_m.group(1))
    received = int(stats_m.group(2))

    if rtt_m:
        min_ms = float(rtt_m.group(1))
        avg_ms = float(rtt_m.group(2))
        max_ms = float(rtt_m.group(3))
        mdev_ms = float(rtt_m.group(4))
    elif received == 0:
        # All packets lost — no RTT data
        min_ms = avg_ms = max_ms = mdev_ms = 0.0
    else:
        log.debug("Could not parse RTT for %s", host)
        return None

    return PingResult(
        host=host,
        sent=actual_sent,
        received=received,
        min_ms=min_ms,
        avg_ms=avg_ms,
        max_ms=max_ms,
        mdev_ms=mdev_ms,
    )


# ── Measurement engine ────────────────────────────────────────────────────────

class MeasurementEngine:
    """
    Background asyncio task that probes link quality and populates the
    MeasurementStore.

    Instantiate and call start(). Call stop() on shutdown.
    """

    def __init__(
        self,
        graph: "RoutingGraph",
        store: "MeasurementStore",
        journal: "MonitoringJournal | None" = None,
        metric_store: "MetricStore | None" = None,
    ) -> None:
        self._graph = graph
        self._store = store
        self._journal = journal
        self._metric_store = metric_store
        self._task: asyncio.Task | None = None
        self._running = False
        # Track consecutive probe failures per link_id
        self._failures: dict[str, int] = {}

    async def start(self) -> None:
        """Start the background measurement loop."""
        if self._task is not None:
            return
        self._running = True
        self._task = asyncio.create_task(
            self._run(), name="routing.measurement_engine"
        )
        log.info("Measurement engine started")

    async def stop(self) -> None:
        """Stop the background measurement loop and wait for it to exit."""
        self._running = False
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None
        log.info("Measurement engine stopped")

    async def probe_link_now(self, link_id: str) -> bool:
        """
        Trigger an immediate probe of a single link by ID.

        Returns True if the link was found and probed, False otherwise.
        """
        link = self._graph.get_link(link_id)
        if link is None:
            return False
        await self._probe_link(link)
        return True

    # ── Internal ──────────────────────────────────────────────────────────────

    async def _run(self) -> None:
        """Main measurement loop."""
        # Stagger initial probes so startup doesn't hit all hosts at once.
        links = list(self._graph.links())
        for i, link in enumerate(links):
            if not self._running:
                return
            if self._link_has_target(link):
                await asyncio.sleep(_STAGGER_S * i)
                await self._probe_link(link)

        # Steady-state: probe each link at its adapted interval.
        while self._running:
            links = list(self._graph.links())
            probe_tasks = [
                self._probe_link(link)
                for link in links
                if self._link_has_target(link)
            ]
            if probe_tasks:
                await asyncio.gather(*probe_tasks, return_exceptions=True)
            await asyncio.sleep(_DEFAULT_INTERVAL_S)

    @staticmethod
    def _link_has_target(link: object) -> bool:
        """True if this link has a probeable network target."""
        return bool(getattr(link, "properties", {}).get("target_ip"))

    async def _probe_link(self, link: object) -> None:
        """Probe a single link and record results in the store."""
        props = getattr(link, "properties", {})
        host: str | None = props.get("target_ip")
        if not host:
            return

        link_id: str = link.id  # type: ignore[union-attr]
        dev_id: str = link.source.device_id  # type: ignore[union-attr]

        log.debug("Probing link %s -> %s", link_id, host)
        result = await _run_ping(host)

        now = time.monotonic()

        if result is None:
            self._failures[link_id] = self._failures.get(link_id, 0) + 1
            self._store.record(  # type: ignore[union-attr]
                dev_id, f"link.{link_id}.probe_error",
                True, InfoQuality.measured,
                source="icmp", refresh_class="network_health", now=now,
            )
            if self._failures[link_id] >= _FAILURE_THRESHOLD:
                self._mark_link_failed(link, link_id)
            return

        # Success — reset failure counter
        self._failures[link_id] = 0

        # Record latency
        self._store.record(  # type: ignore[union-attr]
            dev_id, f"link.{link_id}.latency_ms",
            result.avg_ms, InfoQuality.measured,
            source="icmp", refresh_class="network_health", now=now,
        )
        # Record loss
        self._store.record(  # type: ignore[union-attr]
            dev_id, f"link.{link_id}.loss_rate",
            result.loss_rate, InfoQuality.measured,
            source="icmp", refresh_class="network_health", now=now,
        )
        # Record jitter (p99 estimate)
        self._store.record(  # type: ignore[union-attr]
            dev_id, f"link.{link_id}.jitter_p99_ms",
            result.p99_jitter_ms, InfoQuality.measured,
            source="icmp", refresh_class="network_health", now=now,
        )

        # Also record to MetricStore for time-series history (if wired)
        if self._metric_store is not None:
            ms = self._metric_store
            ms.record(dev_id, f"link.{link_id}.latency_ms", result.avg_ms, now)  # type: ignore[union-attr]
            ms.record(dev_id, f"link.{link_id}.loss_rate", result.loss_rate, now)  # type: ignore[union-attr]
            ms.record(dev_id, f"link.{link_id}.jitter_p99_ms", result.p99_jitter_ms, now)  # type: ignore[union-attr]

        # Update the link's live state metrics so the graph reflects reality.
        self._update_link_state(link, result, now)

        log.debug(
            "Link %s: latency=%.1fms loss=%.1f%% jitter=%.1fms",
            link_id, result.avg_ms, result.loss_rate * 100, result.p99_jitter_ms,
        )

    def _update_link_state(self, link: object, result: PingResult, now: float) -> None:
        """Update the in-graph link state with fresh measurement data."""
        from .model import JitterSpec, LatencySpec, LossSpec

        state = getattr(link, "state", None)
        if state is None:
            return

        state.latency = LatencySpec(
            min_ms=result.min_ms,
            typical_ms=result.avg_ms,
            max_ms=result.max_ms,
            quality=InfoQuality.measured,
        )
        state.loss = LossSpec(
            rate=result.loss_rate,
            window_seconds=int(_PING_COUNT * _PING_TIMEOUT_S),
            quality=InfoQuality.measured,
        )
        state.jitter = JitterSpec(
            mean_ms=result.mdev_ms,
            p95_ms=result.mdev_ms * 1.65,
            p99_ms=result.p99_jitter_ms,
            quality=InfoQuality.measured,
        )
        state.last_measured = now

        # Recover link status if it was previously marked failed
        if state.status == LinkStatus.failed:
            state.status = LinkStatus.active
            if self._journal is not None:
                from .monitoring import StateChangeRecord, StateChangeType
                self._journal.append(StateChangeRecord(
                    type=StateChangeType.link_recovered,
                    device_id=link.source.device_id,  # type: ignore[union-attr]
                    message=(
                        f"Link {link.id} recovered "  # type: ignore[union-attr]
                        f"(latency={result.avg_ms:.1f}ms)"
                    ),
                    source="measurement_engine",
                    severity="info",
                ))

    def _mark_link_failed(self, link: object, link_id: str) -> None:
        """Mark a link as failed after repeated probe failures."""
        state = getattr(link, "state", None)
        if state is None:
            return
        if state.status == LinkStatus.failed:
            return  # already marked; avoid duplicate journal entries

        state.status = LinkStatus.failed
        log.warning("Link %s marked failed after %d consecutive probe failures",
                    link_id, _FAILURE_THRESHOLD)

        if self._journal is not None:
            from .monitoring import StateChangeRecord, StateChangeType
            self._journal.append(StateChangeRecord(
                type=StateChangeType.link_down,
                device_id=link.source.device_id,  # type: ignore[union-attr]
                message=(
                    f"Link {link_id} marked failed after "
                    f"{_FAILURE_THRESHOLD} consecutive probe failures"
                ),
                source="measurement_engine",
                severity="warning",
            ))

    def to_dict(self) -> dict:
        return {
            "running": self._running,
            "task_active": self._task is not None and not self._task.done(),
            "failure_counts": dict(self._failures),
        }
