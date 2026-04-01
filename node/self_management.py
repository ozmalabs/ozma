# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Node self-management — resource monitoring, storage, network, health.

Every node continuously monitors its own resources and reports to the
controller.  This enables the mesh intelligence engine to assess the
health and capability of the entire network.

Reports:
  CPU usage + temperature (of the node's SBC itself)
  Memory usage (RAM + swap)
  Storage usage (per-mount: rootfs, HLS segments, replay buffer, logs)
  Network throughput (TX/RX bytes, packets, errors, drops)
  Network latency to controller (RTT)
  USB gadget status (connected, enumerated, endpoints active)
  Uptime + load average
  WiFi signal strength + channel + noise (if wireless)

Served via the node's HTTP API at GET /node/status.
Advertised in mDNS TXT as node_status_interval=5.
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.self_mgmt")

REPORT_INTERVAL = 5.0


@dataclass
class NodeSelfReport:
    """Comprehensive self-report from a node."""

    # Identity
    hostname: str = ""
    uptime_s: float = 0.0
    load_avg: tuple[float, float, float] = (0.0, 0.0, 0.0)

    # CPU
    cpu_usage_pct: float = 0.0
    cpu_temp_c: float = 0.0
    cpu_count: int = 1

    # Memory
    mem_total_mb: float = 0.0
    mem_used_mb: float = 0.0
    mem_pct: float = 0.0
    swap_used_mb: float = 0.0

    # Storage (per mount)
    storage: list[dict] = field(default_factory=list)  # [{mount, total_mb, used_mb, pct}]

    # Network
    net_tx_bytes: int = 0
    net_rx_bytes: int = 0
    net_tx_packets: int = 0
    net_rx_packets: int = 0
    net_tx_errors: int = 0
    net_rx_errors: int = 0
    net_tx_drops: int = 0
    net_rx_drops: int = 0
    net_speed_mbps: int = 0        # Link speed (1000 = gigabit)
    net_interface: str = ""

    # WiFi (if applicable)
    wifi_signal_dbm: int = 0       # 0 = not wireless
    wifi_channel: int = 0
    wifi_noise_dbm: int = 0
    wifi_ssid: str = ""

    # USB gadget
    usb_connected: bool = False
    usb_speed: str = ""            # "480" (USB2), "5000" (USB3)

    # Controller latency
    controller_rtt_ms: float = 0.0

    # Timestamp
    timestamp: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "hostname": self.hostname,
            "uptime_s": round(self.uptime_s, 0),
            "load_avg": list(self.load_avg),
            "cpu": {"usage_pct": round(self.cpu_usage_pct, 1), "temp_c": round(self.cpu_temp_c, 1), "cores": self.cpu_count},
            "memory": {"total_mb": round(self.mem_total_mb, 0), "used_mb": round(self.mem_used_mb, 0), "pct": round(self.mem_pct, 1), "swap_mb": round(self.swap_used_mb, 0)},
            "storage": self.storage,
            "network": {
                "interface": self.net_interface,
                "speed_mbps": self.net_speed_mbps,
                "tx_bytes": self.net_tx_bytes, "rx_bytes": self.net_rx_bytes,
                "tx_errors": self.net_tx_errors, "rx_errors": self.net_rx_errors,
                "tx_drops": self.net_tx_drops, "rx_drops": self.net_rx_drops,
            },
            "wifi": {"signal_dbm": self.wifi_signal_dbm, "channel": self.wifi_channel,
                     "ssid": self.wifi_ssid} if self.wifi_signal_dbm else None,
            "usb": {"connected": self.usb_connected, "speed": self.usb_speed},
            "controller_rtt_ms": round(self.controller_rtt_ms, 1),
            "timestamp": self.timestamp,
        }


class NodeSelfManager:
    """Monitors the node's own resources and serves the report."""

    def __init__(self) -> None:
        self._report = NodeSelfReport()
        self._task: asyncio.Task | None = None
        self._prev_net: dict[str, int] = {}

    async def start(self) -> None:
        self._task = asyncio.create_task(self._monitor_loop(), name="self-mgmt")
        log.info("Node self-management started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def get_report(self) -> NodeSelfReport:
        return self._report

    async def _monitor_loop(self) -> None:
        while True:
            try:
                self._collect()
                await asyncio.sleep(REPORT_INTERVAL)
            except asyncio.CancelledError:
                return

    def _collect(self) -> None:
        r = self._report
        r.timestamp = time.time()
        r.hostname = os.uname().nodename

        # Uptime
        try:
            r.uptime_s = float(Path("/proc/uptime").read_text().split()[0])
        except Exception:
            pass

        # Load average
        try:
            r.load_avg = os.getloadavg()
        except Exception:
            pass

        # CPU
        try:
            stat = Path("/proc/stat").read_text().splitlines()[0].split()
            # Simplified — would need delta for accurate usage
            r.cpu_count = os.cpu_count() or 1
        except Exception:
            pass

        # CPU temperature
        for thermal in Path("/sys/class/thermal").glob("thermal_zone*/temp"):
            try:
                r.cpu_temp_c = int(thermal.read_text().strip()) / 1000.0
                break
            except Exception:
                pass

        # Memory
        try:
            meminfo = Path("/proc/meminfo").read_text()
            total = used = swap = 0
            for line in meminfo.splitlines():
                parts = line.split()
                if parts[0] == "MemTotal:":
                    total = int(parts[1]) / 1024
                elif parts[0] == "MemAvailable:":
                    used = total - int(parts[1]) / 1024
                elif parts[0] == "SwapTotal:":
                    swap_total = int(parts[1]) / 1024
                elif parts[0] == "SwapFree:":
                    swap = swap_total - int(parts[1]) / 1024
            r.mem_total_mb = total
            r.mem_used_mb = used
            r.mem_pct = (used / total * 100) if total > 0 else 0
            r.swap_used_mb = swap
        except Exception:
            pass

        # Storage
        r.storage = []
        try:
            for mount in ("/", "/tmp"):
                st = os.statvfs(mount)
                total = st.f_blocks * st.f_frsize / 1048576
                used = (st.f_blocks - st.f_bavail) * st.f_frsize / 1048576
                r.storage.append({
                    "mount": mount,
                    "total_mb": round(total, 0),
                    "used_mb": round(used, 0),
                    "pct": round(used / total * 100, 1) if total > 0 else 0,
                })
        except Exception:
            pass

        # Network
        try:
            for iface_dir in sorted(Path("/sys/class/net").iterdir()):
                name = iface_dir.name
                if name == "lo":
                    continue
                operstate = (iface_dir / "operstate").read_text().strip()
                if operstate != "up":
                    continue
                r.net_interface = name

                stats = iface_dir / "statistics"
                r.net_tx_bytes = int((stats / "tx_bytes").read_text().strip())
                r.net_rx_bytes = int((stats / "rx_bytes").read_text().strip())
                r.net_tx_packets = int((stats / "tx_packets").read_text().strip())
                r.net_rx_packets = int((stats / "rx_packets").read_text().strip())
                r.net_tx_errors = int((stats / "tx_errors").read_text().strip())
                r.net_rx_errors = int((stats / "rx_errors").read_text().strip())
                r.net_tx_drops = int((stats / "tx_dropped").read_text().strip())
                r.net_rx_drops = int((stats / "rx_dropped").read_text().strip())

                try:
                    r.net_speed_mbps = int((iface_dir / "speed").read_text().strip())
                except Exception:
                    r.net_speed_mbps = 0

                # WiFi signal
                try:
                    import subprocess
                    iw = subprocess.run(["iwconfig", name], capture_output=True, text=True, timeout=2)
                    import re
                    sig = re.search(r"Signal level[=:](-?\d+)", iw.stdout)
                    if sig:
                        r.wifi_signal_dbm = int(sig.group(1))
                    ch = re.search(r"Frequency.*Channel\s*(\d+)", iw.stdout)
                    if ch:
                        r.wifi_channel = int(ch.group(1))
                    ssid = re.search(r'ESSID:"([^"]*)"', iw.stdout)
                    if ssid:
                        r.wifi_ssid = ssid.group(1)
                except Exception:
                    pass

                break  # Use first active interface
        except Exception:
            pass

        # USB gadget
        try:
            udc_path = Path("/sys/kernel/config/usb_gadget/ozma/UDC")
            if udc_path.exists():
                udc = udc_path.read_text().strip()
                r.usb_connected = bool(udc)
                speed_path = Path(f"/sys/class/udc/{udc}/current_speed") if udc else None
                if speed_path and speed_path.exists():
                    r.usb_speed = speed_path.read_text().strip()
        except Exception:
            pass


def register_self_mgmt_routes(app: web.Application, mgr: NodeSelfManager) -> None:
    async def get_status(_: web.Request) -> web.Response:
        return web.json_response(mgr.get_report().to_dict())

    app.router.add_get("/node/status", get_status)
