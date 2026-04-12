# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Network health monitoring — latency, packet loss, and bandwidth per node.

Periodically pings each known node and tracks:
  - Round-trip latency (ms)
  - Packet loss percentage
  - Jitter (variation in latency)
  - Connection uptime

Results are available via API for the dashboard network health panel.
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.network_health")

PROBE_INTERVAL = 5.0    # seconds between probes
HISTORY_SIZE = 120       # 10 minutes at 5s intervals
PING_TIMEOUT = 2.0


@dataclass
class NodeHealth:
    """Health metrics for a single node."""
    node_id: str
    host: str
    latency_ms: float = 0.0
    packet_loss: float = 0.0
    jitter_ms: float = 0.0
    p99_latency_ms: float = 0.0
    last_seen: float = 0.0
    online: bool = False
    history: deque[tuple[float, float]] = field(
        default_factory=lambda: deque(maxlen=HISTORY_SIZE)
    )  # (timestamp, latency_ms) — -1 = timeout

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id": self.node_id,
            "host": self.host,
            "latency_ms": round(self.latency_ms, 1),
            "packet_loss": round(self.packet_loss, 1),
            "jitter_ms": round(self.jitter_ms, 1),
            "p99_latency_ms": round(self.p99_latency_ms, 1),
            "online": self.online,
        }


class NetworkHealthMonitor:
    """Monitors network health to all known nodes."""

    def __init__(self, state: Any) -> None:
        self._state = state
        self._health: dict[str, NodeHealth] = {}
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._probe_loop(), name="net-health")
        log.info("Network health monitor started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def list_health(self) -> list[dict[str, Any]]:
        return [h.to_dict() for h in self._health.values()]

    def get_health(self, node_id: str) -> dict[str, Any] | None:
        h = self._health.get(node_id)
        return h.to_dict() if h else None

    def get_history(self, node_id: str) -> list[tuple[float, float]]:
        h = self._health.get(node_id)
        return list(h.history) if h else []

    async def _probe_loop(self) -> None:
        while True:
            try:
                # Probe all known nodes in parallel
                tasks = []
                for node in list(self._state.nodes.values()):
                    if node.id not in self._health:
                        self._health[node.id] = NodeHealth(node_id=node.id, host=node.host)
                    self._health[node.id].host = node.host
                    tasks.append(self._probe_node(self._health[node.id]))

                if tasks:
                    await asyncio.gather(*tasks, return_exceptions=True)

                # Remove stale entries for nodes no longer in state
                stale = [nid for nid in self._health if nid not in self._state.nodes]
                for nid in stale:
                    del self._health[nid]

                await asyncio.sleep(PROBE_INTERVAL)
            except asyncio.CancelledError:
                return

    async def _probe_node(self, health: NodeHealth) -> None:
        """Measure latency to a node via TCP connect to its API port."""
        node = self._state.nodes.get(health.node_id)
        port = node.api_port or node.port if node else 0
        if not port:
            return

        start = time.monotonic()
        try:
            _, writer = await asyncio.wait_for(
                asyncio.open_connection(health.host, port),
                timeout=PING_TIMEOUT,
            )
            latency = (time.monotonic() - start) * 1000
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

            health.latency_ms = latency
            health.online = True
            health.last_seen = time.time()
            health.history.append((time.time(), latency))

        except (asyncio.TimeoutError, OSError, ConnectionRefusedError):
            health.online = False
            health.history.append((time.time(), -1))

        # Calculate packet loss and jitter from history
        recent = list(health.history)[-20:]  # Last ~100 seconds
        if recent:
            losses = sum(1 for _, lat in recent if lat < 0)
            health.packet_loss = (losses / len(recent)) * 100

            latencies = [lat for _, lat in recent if lat >= 0]
            if len(latencies) >= 2:
                diffs = [abs(latencies[i] - latencies[i-1]) for i in range(1, len(latencies))]
                health.jitter_ms = sum(diffs) / len(diffs)
            
            # Calculate 99th percentile latency
            if latencies:
                latencies_sorted = sorted(latencies)
                index = int(0.99 * (len(latencies_sorted) - 1))
                health.p99_latency_ms = latencies_sorted[index]
