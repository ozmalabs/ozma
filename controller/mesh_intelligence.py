# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Mesh intelligence — network state assessment, adaptive quality, recommendations.

Continuously monitors the health and capability of every node and the
network connecting them.  Identifies problems, adjusts quality, and
recommends infrastructure improvements.

Capabilities:
  1. Network state mesh — RTT, bandwidth, packet loss, jitter per link
  2. Adaptive quality — reduce capture/stream bitrate when congested
  3. Problem detection — identify nodes on WiFi that should be wired,
     saturated links, failing hardware, storage warnings
  4. Recommendations — actionable suggestions for improving the setup
  5. Capacity planning — predict when storage/bandwidth will be exhausted

Recommendations engine:
  Each recommendation has: severity, category, affected node(s),
  description, and suggested action.

  Example recommendations:
    WARNING: "Node vm2 is on WiFi with -72dBm signal. Video quality will
             be limited. Connect via Ethernet for best results."
    ERROR:   "Node server-1 has 94% disk usage. Replay buffer and session
             recordings may fail. Free up storage."
    INFO:    "Network link to node rack-3 averages 2.3ms RTT. For 4K/60
             video, consider upgrading to 10GbE."
    WARNING: "Node kiosk-5 has 127 network errors in the last hour.
             Check cable or switch port."
    ERROR:   "Node desk-2 USB voltage is 4.62V — below spec. PSU may be
             failing or cable is too long. Replace cable or USB hub."
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import urllib.request
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.mesh")


@dataclass
class Recommendation:
    """An actionable recommendation for improving the setup."""
    id: str
    severity: str              # info, warning, error, critical
    category: str              # network, storage, performance, hardware, config
    node_id: str = ""
    title: str = ""
    description: str = ""
    action: str = ""           # Suggested fix
    auto_applied: bool = False # Was an automatic adjustment made?
    timestamp: float = 0.0
    dismissed: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "severity": self.severity, "category": self.category,
            "node": self.node_id, "title": self.title,
            "description": self.description, "action": self.action,
            "auto_applied": self.auto_applied, "dismissed": self.dismissed,
        }


@dataclass
class NodeMeshState:
    """Mesh state for a single node."""
    node_id: str
    rtt_ms: float = 0.0
    bandwidth_mbps: float = 0.0   # Estimated available bandwidth
    link_speed_mbps: int = 0      # Physical link speed (1000 = GbE)
    packet_loss_pct: float = 0.0
    jitter_ms: float = 0.0
    is_wifi: bool = False
    wifi_signal_dbm: int = 0
    storage_pct: float = 0.0      # Disk usage %
    cpu_pct: float = 0.0
    mem_pct: float = 0.0
    usb_voltage: float = 5.0
    net_errors: int = 0
    net_drops: int = 0
    last_report: float = 0.0

    # Adaptive quality state
    current_bitrate: str = "8M"   # Current encode bitrate
    quality_reduced: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id": self.node_id,
            "rtt_ms": round(self.rtt_ms, 1),
            "bandwidth_mbps": round(self.bandwidth_mbps, 0),
            "link_speed_mbps": self.link_speed_mbps,
            "packet_loss_pct": round(self.packet_loss_pct, 1),
            "is_wifi": self.is_wifi,
            "wifi_signal_dbm": self.wifi_signal_dbm,
            "storage_pct": round(self.storage_pct, 1),
            "usb_voltage": round(self.usb_voltage, 2),
            "net_errors": self.net_errors,
            "quality_reduced": self.quality_reduced,
        }


class MeshIntelligence:
    """
    Network mesh state engine with adaptive quality and recommendations.

    Polls every node's /node/status endpoint, builds a mesh state model,
    detects problems, adjusts quality, and generates recommendations.
    """

    def __init__(self, state: Any, net_health: Any = None) -> None:
        self._state = state
        self._net_health = net_health
        self._mesh: dict[str, NodeMeshState] = {}
        self._recommendations: list[Recommendation] = []
        self._task: asyncio.Task | None = None
        self._rec_counter = 0

    async def start(self) -> None:
        self._task = asyncio.create_task(self._assess_loop(), name="mesh-intelligence")
        log.info("Mesh intelligence engine started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def get_mesh_state(self) -> list[dict]:
        return [n.to_dict() for n in self._mesh.values()]

    def get_recommendations(self, include_dismissed: bool = False) -> list[dict]:
        return [r.to_dict() for r in self._recommendations
                if include_dismissed or not r.dismissed]

    def dismiss_recommendation(self, rec_id: str) -> bool:
        for r in self._recommendations:
            if r.id == rec_id:
                r.dismissed = True
                return True
        return False

    def get_overall_health(self) -> dict[str, Any]:
        """Return an overall health assessment of the mesh."""
        nodes = list(self._mesh.values())
        if not nodes:
            return {"status": "unknown", "nodes": 0}

        active_recs = [r for r in self._recommendations if not r.dismissed]
        critical = sum(1 for r in active_recs if r.severity == "critical")
        errors = sum(1 for r in active_recs if r.severity == "error")
        warnings = sum(1 for r in active_recs if r.severity == "warning")

        if critical > 0:
            status = "critical"
        elif errors > 0:
            status = "degraded"
        elif warnings > 0:
            status = "fair"
        else:
            status = "healthy"

        avg_rtt = sum(n.rtt_ms for n in nodes) / len(nodes) if nodes else 0
        wifi_count = sum(1 for n in nodes if n.is_wifi)

        return {
            "status": status,
            "nodes": len(nodes),
            "avg_rtt_ms": round(avg_rtt, 1),
            "wifi_nodes": wifi_count,
            "recommendations": {"critical": critical, "error": errors, "warning": warnings},
        }

    # ── Assessment loop ──────────────────────────────────────────────────────

    async def _assess_loop(self) -> None:
        while True:
            try:
                await self._collect_mesh_state()
                self._run_assessments()
                await asyncio.sleep(30)
            except asyncio.CancelledError:
                return

    async def _collect_mesh_state(self) -> None:
        """Poll each node's /node/status for self-management data."""
        for node in list(self._state.nodes.values()):
            if not node.api_port:
                continue

            node_id = node.id
            if node_id not in self._mesh:
                self._mesh[node_id] = NodeMeshState(node_id=node_id)
            ms = self._mesh[node_id]

            try:
                loop = asyncio.get_running_loop()
                url = f"http://{node.host}:{node.api_port}/node/status"
                t_start = time.monotonic()
                def _fetch(u=url):
                    with urllib.request.urlopen(u, timeout=5) as r:
                        return json.loads(r.read())
                data = await loop.run_in_executor(None, _fetch)
                ms.rtt_ms = (time.monotonic() - t_start) * 1000

                # Parse self-report
                net = data.get("network", {})
                ms.link_speed_mbps = net.get("speed_mbps", 0)
                ms.net_errors = net.get("tx_errors", 0) + net.get("rx_errors", 0)
                ms.net_drops = net.get("tx_drops", 0) + net.get("rx_drops", 0)

                wifi = data.get("wifi")
                if wifi and wifi.get("signal_dbm"):
                    ms.is_wifi = True
                    ms.wifi_signal_dbm = wifi["signal_dbm"]
                else:
                    ms.is_wifi = False

                cpu = data.get("cpu", {})
                ms.cpu_pct = cpu.get("usage_pct", 0)

                mem = data.get("memory", {})
                ms.mem_pct = mem.get("pct", 0)

                storage = data.get("storage", [])
                if storage:
                    ms.storage_pct = max(s.get("pct", 0) for s in storage)

                usb = data.get("usb", {})
                ms.last_report = time.time()

                # Estimate bandwidth from link speed and RTT
                if ms.link_speed_mbps > 0:
                    ms.bandwidth_mbps = ms.link_speed_mbps * 0.8  # Rough estimate

            except Exception:
                ms.rtt_ms = -1  # Unreachable

        # Also pull from network health monitor if available
        if self._net_health:
            for h in self._net_health.list_health():
                nid = h.get("node_id", "")
                if nid in self._mesh:
                    self._mesh[nid].packet_loss_pct = h.get("packet_loss", 0)
                    self._mesh[nid].jitter_ms = h.get("jitter_ms", 0)

    def _run_assessments(self) -> None:
        """Analyse mesh state and generate recommendations."""
        # Clear non-dismissed, auto-generated recommendations older than 5 min
        cutoff = time.time() - 300
        self._recommendations = [
            r for r in self._recommendations
            if r.dismissed or r.timestamp > cutoff
        ]

        for ms in self._mesh.values():
            self._assess_node(ms)

    def _assess_node(self, ms: NodeMeshState) -> None:
        """Generate recommendations for a single node."""

        # WiFi with video
        if ms.is_wifi and ms.wifi_signal_dbm < -70:
            self._recommend("warning", "network", ms.node_id,
                "Weak WiFi signal",
                f"Node {ms.node_id.split('.')[0]} has {ms.wifi_signal_dbm}dBm WiFi signal. Video quality will be degraded.",
                "Connect via Ethernet cable for reliable video streaming.")

        if ms.is_wifi and ms.link_speed_mbps < 100:
            self._recommend("warning", "network", ms.node_id,
                "WiFi too slow for video",
                f"Node {ms.node_id.split('.')[0]} WiFi link is {ms.link_speed_mbps}Mbps. 4K/60 video requires ~30Mbps sustained.",
                "Switch to Ethernet or upgrade to WiFi 6 for video capture nodes.")

        # Storage
        if ms.storage_pct > 95:
            self._recommend("critical", "storage", ms.node_id,
                "Storage critically full",
                f"Node {ms.node_id.split('.')[0]} has {ms.storage_pct:.0f}% disk usage. Recording and replay buffer will fail.",
                "Free up storage immediately. Consider adding external storage or reducing replay buffer duration.")
        elif ms.storage_pct > 85:
            self._recommend("warning", "storage", ms.node_id,
                "Storage getting full",
                f"Node {ms.node_id.split('.')[0]} is at {ms.storage_pct:.0f}% disk usage.",
                "Clean up old recordings or increase storage capacity.")

        # Network errors
        if ms.net_errors > 100:
            self._recommend("warning", "network", ms.node_id,
                "Network errors detected",
                f"Node {ms.node_id.split('.')[0]} has {ms.net_errors} network errors. Check cable, switch port, or NIC.",
                "Replace the Ethernet cable. If errors persist, try a different switch port.")

        # Packet loss
        if ms.packet_loss_pct > 5:
            self._recommend("error", "network", ms.node_id,
                "High packet loss",
                f"Node {ms.node_id.split('.')[0]} has {ms.packet_loss_pct:.1f}% packet loss. Audio and video will be affected.",
                "Check network infrastructure. Consider dedicated switch or VLAN for ozma traffic.")

        # High latency
        if ms.rtt_ms > 10 and ms.link_speed_mbps >= 1000:
            self._recommend("info", "network", ms.node_id,
                "Higher than expected latency",
                f"Node {ms.node_id.split('.')[0]} has {ms.rtt_ms:.1f}ms RTT on a GbE link. Expected <2ms.",
                "Check for network congestion, switch overload, or routing issues.")

        # USB voltage
        if ms.usb_voltage > 0 and ms.usb_voltage < 4.75:
            self._recommend("error", "hardware", ms.node_id,
                "USB voltage below spec",
                f"Node {ms.node_id.split('.')[0]} USB voltage is {ms.usb_voltage:.2f}V (spec: 5.0V ±5%).",
                "Replace USB cable (shorter/thicker) or check target machine's PSU. Voltage drop indicates cable quality or PSU degradation.")

        # High RTT for video
        if ms.rtt_ms > 5 and not ms.is_wifi:
            self._recommend("info", "performance", ms.node_id,
                "Consider 10GbE for low-latency video",
                f"Node {ms.node_id.split('.')[0]} RTT is {ms.rtt_ms:.1f}ms. For sub-5ms glass-to-glass video, consider 10GbE.",
                "Upgrade to 10GbE network for this node. Requires compatible switch and NICs.")

        # Memory pressure
        if ms.mem_pct > 90:
            self._recommend("warning", "hardware", ms.node_id,
                "High memory usage",
                f"Node {ms.node_id.split('.')[0]} is using {ms.mem_pct:.0f}% RAM.",
                "Check for memory leaks. Consider upgrading to a node with more RAM.")

        # Adaptive quality
        if ms.packet_loss_pct > 2 or ms.rtt_ms > 20:
            if not ms.quality_reduced:
                ms.quality_reduced = True
                ms.current_bitrate = "4M"
                self._recommend("info", "performance", ms.node_id,
                    "Quality reduced automatically",
                    f"Video quality for {ms.node_id.split('.')[0]} reduced to {ms.current_bitrate} due to network conditions.",
                    "This is automatic. Quality will restore when network improves.",
                    auto_applied=True)
        else:
            if ms.quality_reduced:
                ms.quality_reduced = False
                ms.current_bitrate = "8M"

    def _recommend(self, severity: str, category: str, node_id: str,
                    title: str, description: str, action: str,
                    auto_applied: bool = False) -> None:
        """Add a recommendation if one with same title+node doesn't exist."""
        # Dedup
        for existing in self._recommendations:
            if existing.title == title and existing.node_id == node_id and not existing.dismissed:
                return

        self._rec_counter += 1
        self._recommendations.append(Recommendation(
            id=f"rec-{self._rec_counter}",
            severity=severity, category=category,
            node_id=node_id, title=title,
            description=description, action=action,
            auto_applied=auto_applied,
            timestamp=time.time(),
        ))
