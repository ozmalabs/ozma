# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Connect client for nodes.

Nodes register directly with Connect, independent of the controller.
This gives two critical capabilities:

  1. **Mesh visibility** — Connect sees every node even if the controller
     is offline. The dashboard shows the full mesh in real time.

  2. **Remote access** — each node gets its own WireGuard tunnel through
     the Connect relay. A node in a colo, a remote office, or a different
     continent appears as just another node in the mesh. The controller
     routes HID to it over WireGuard. Video streams back.

     This means you can KVM switch to your colo server from your desk.
     Same keyboard shortcut. Same UI. Same latency profile as any other
     node (plus the WireGuard RTT).

The node advertises the same mDNS TXT fields to the controller AND
registers with Connect. The controller discovers it either way — local
mDNS for LAN nodes, Connect relay for remote nodes.

No metering — nodes don't gate any features locally.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import shutil
import subprocess
import time
from dataclasses import dataclass, field

log = logging.getLogger("ozma.node.connect")

CONNECT_API_BASE = "https://connect.ozma.dev/api/v1"


@dataclass
class ConnectionState:
    """
    Live connection metrics for this node.

    Tracked continuously and exposed to the controller via the node's
    HTTP API. The controller uses this for:
      - Dashboard overlays (latency badge on each node)
      - Alarms (packet loss > threshold → notification)
      - Adaptive quality (high jitter → reduce stream bitrate)
      - Network health panel in the web UI
    """
    # Controller link
    controller_rtt_ms: float = 0.0       # Round-trip to controller (HID path)
    controller_packet_loss: float = 0.0  # 0.0-1.0
    controller_jitter_ms: float = 0.0

    # Connect relay link
    relay_rtt_ms: float = 0.0            # Round-trip to relay server
    relay_connected: bool = False
    relay_ip: str = ""

    # Connect cloud link
    connect_rtt_ms: float = 0.0          # Round-trip to Connect API
    connect_reachable: bool = False

    # General
    uptime_s: float = 0.0
    last_hid_packet_at: float = 0.0      # timestamp of last HID packet received
    hid_packets_received: int = 0
    hid_packets_per_second: float = 0.0

    def to_dict(self) -> dict:
        return {
            "controller_rtt_ms": round(self.controller_rtt_ms, 1),
            "controller_packet_loss": round(self.controller_packet_loss, 3),
            "controller_jitter_ms": round(self.controller_jitter_ms, 1),
            "relay_rtt_ms": round(self.relay_rtt_ms, 1),
            "relay_connected": self.relay_connected,
            "relay_ip": self.relay_ip,
            "connect_rtt_ms": round(self.connect_rtt_ms, 1),
            "connect_reachable": self.connect_reachable,
            "uptime_s": round(self.uptime_s, 0),
            "last_hid_packet_at": self.last_hid_packet_at,
            "hid_packets_received": self.hid_packets_received,
            "hid_pps": round(self.hid_packets_per_second, 1),
        }

    def to_prometheus(self, node_id: str = "") -> str:
        """Render metrics in Prometheus exposition format."""
        label = f'node="{node_id}"' if node_id else ""
        lb = "{" + label + "}" if label else ""
        lines = [
            f"# HELP ozma_node_controller_rtt_ms RTT to controller in milliseconds",
            f"# TYPE ozma_node_controller_rtt_ms gauge",
            f"ozma_node_controller_rtt_ms{lb} {self.controller_rtt_ms:.1f}",
            f"# HELP ozma_node_controller_packet_loss Packet loss ratio to controller",
            f"# TYPE ozma_node_controller_packet_loss gauge",
            f"ozma_node_controller_packet_loss{lb} {self.controller_packet_loss:.4f}",
            f"# HELP ozma_node_controller_jitter_ms Jitter to controller in milliseconds",
            f"# TYPE ozma_node_controller_jitter_ms gauge",
            f"ozma_node_controller_jitter_ms{lb} {self.controller_jitter_ms:.1f}",
            f"# HELP ozma_node_relay_rtt_ms RTT to Connect relay in milliseconds",
            f"# TYPE ozma_node_relay_rtt_ms gauge",
            f"ozma_node_relay_rtt_ms{lb} {self.relay_rtt_ms:.1f}",
            f"# HELP ozma_node_relay_connected Whether the relay tunnel is up",
            f"# TYPE ozma_node_relay_connected gauge",
            f"ozma_node_relay_connected{lb} {int(self.relay_connected)}",
            f"# HELP ozma_node_connect_rtt_ms RTT to Connect API in milliseconds",
            f"# TYPE ozma_node_connect_rtt_ms gauge",
            f"ozma_node_connect_rtt_ms{lb} {self.connect_rtt_ms:.1f}",
            f"# HELP ozma_node_connect_reachable Whether Connect API is reachable",
            f"# TYPE ozma_node_connect_reachable gauge",
            f"ozma_node_connect_reachable{lb} {int(self.connect_reachable)}",
            f"# HELP ozma_node_uptime_seconds Node uptime in seconds",
            f"# TYPE ozma_node_uptime_seconds counter",
            f"ozma_node_uptime_seconds{lb} {self.uptime_s:.0f}",
            f"# HELP ozma_node_hid_packets_total Total HID packets received",
            f"# TYPE ozma_node_hid_packets_total counter",
            f"ozma_node_hid_packets_total{lb} {self.hid_packets_received}",
            f"# HELP ozma_node_hid_packets_per_second Current HID packet rate",
            f"# TYPE ozma_node_hid_packets_per_second gauge",
            f"ozma_node_hid_packets_per_second{lb} {self.hid_packets_per_second:.1f}",
        ]
        return "\n".join(lines) + "\n"


class NodeConnectClient:
    """
    Connect client for nodes (soft, hardware, virtual, desktop).

    Handles:
      - Registration with Connect (mesh visibility)
      - WireGuard tunnel setup (remote access)
      - Heartbeats (online/offline tracking)
    """

    def __init__(self, node_id: str, node_type: str = "soft",
                 hid_port: int = 0, api_base: str = "") -> None:
        self._api_base = (api_base or os.environ.get(
            "OZMA_CONNECT_API", CONNECT_API_BASE)).rstrip("/")
        self._token = os.environ.get("OZMA_CONNECT_TOKEN", "")
        self._node_id = node_id
        self._node_type = node_type  # soft, hardware, virtual, desktop
        self._hid_port = hid_port    # UDP port for HID packets
        self._heartbeat_task: asyncio.Task | None = None
        self._metrics_task: asyncio.Task | None = None
        self._registered = False

        # Relay state
        self._relay_ip: str = ""        # WireGuard IP assigned by relay
        self._relay_endpoint: str = ""  # Relay server endpoint
        self._wg_interface: str = ""    # WireGuard interface name (ozma-relay)

        # Connection metrics — exposed to the controller for dashboards/alarms
        self.state = ConnectionState()

    @property
    def enabled(self) -> bool:
        """Connect registration only happens if a token is configured."""
        return bool(self._token)

    @property
    def relay_ip(self) -> str:
        """WireGuard IP assigned by the relay, if connected."""
        return self._relay_ip

    async def start(self, capabilities: str = "", version: str = "",
                     extra: dict | None = None) -> None:
        """Register with Connect, set up relay tunnel, start heartbeat."""
        if not self.enabled:
            return

        ok = await self._register(capabilities, version, extra or {})
        if ok:
            # Set up WireGuard relay tunnel for remote access
            await self._setup_relay()

            self._heartbeat_task = asyncio.create_task(
                self._heartbeat_loop(),
                name=f"connect-heartbeat-{self._node_id}",
            )

        # Always start metrics collection (even without Connect)
        self._metrics_task = asyncio.create_task(
            self._metrics_loop(),
            name=f"connect-metrics-{self._node_id}",
        )

    def record_hid_packet(self) -> None:
        """Called by the node when an HID packet is received from the controller."""
        self.state.hid_packets_received += 1
        self.state.last_hid_packet_at = time.time()

    async def stop(self) -> None:
        if self._metrics_task:
            self._metrics_task.cancel()
        if self._heartbeat_task:
            self._heartbeat_task.cancel()
        if self._wg_interface:
            await self._teardown_relay()
        if self.enabled and self._registered:
            await self._post("/nodes/heartbeat", {
                "node_id": self._node_id,
                "status": "offline",
            })

    # ── Registration ───────────────────────────────────────────────────────

    async def _register(self, capabilities: str, version: str,
                          extra: dict) -> bool:
        """Register this node with Connect."""
        body = {
            "node_id": self._node_id,
            "node_type": self._node_type,
            "capabilities": capabilities,
            "version": version,
            "platform": platform.system(),
            "arch": platform.machine(),
            "hostname": platform.node(),
            "hid_port": self._hid_port,
            "status": "online",
            **extra,
        }

        result = await self._post("/nodes/register", body)
        if result and result.get("ok"):
            self._registered = True
            log.info("Registered with Connect: %s (%s)", self._node_id, self._node_type)
            return True

        log.debug("Connect registration failed (token may be invalid or Connect unreachable)")
        return False

    # ── WireGuard relay ────────────────────────────────────────────────────

    async def _setup_relay(self) -> None:
        """
        Set up a WireGuard tunnel to the Connect relay.

        This gives the node a routable IP on the ozma mesh. The controller
        can send HID packets to it, and it can stream video back, even
        across the internet.
        """
        if not shutil.which("wg"):
            log.debug("WireGuard not available — relay disabled")
            return

        # Generate a keypair for this node
        try:
            privkey_proc = await asyncio.create_subprocess_exec(
                "wg", "genkey",
                stdout=asyncio.subprocess.PIPE,
            )
            privkey_out, _ = await privkey_proc.communicate()
            privkey = privkey_out.decode().strip()

            pubkey_proc = await asyncio.create_subprocess_exec(
                "wg", "pubkey",
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
            )
            pubkey_out, _ = await pubkey_proc.communicate(privkey.encode())
            pubkey = pubkey_out.decode().strip()
        except Exception as e:
            log.debug("WireGuard keygen failed: %s", e)
            return

        # Register the public key with the relay
        result = await self._post("/relay/register-node", {
            "node_id": self._node_id,
            "wg_public_key": pubkey,
            "hid_port": self._hid_port,
        })
        if not result or not result.get("ok"):
            log.debug("Relay registration failed")
            return

        self._relay_ip = result.get("assigned_ip", "")
        self._relay_endpoint = result.get("relay_endpoint", "")
        relay_pubkey = result.get("relay_public_key", "")
        allowed_ips = result.get("allowed_ips", "10.100.0.0/16")

        if not self._relay_ip or not self._relay_endpoint or not relay_pubkey:
            log.debug("Relay config incomplete")
            return

        # Configure WireGuard interface
        self._wg_interface = "ozma-relay"
        try:
            cmds = [
                ["ip", "link", "add", self._wg_interface, "type", "wireguard"],
                ["ip", "addr", "add", f"{self._relay_ip}/16", "dev", self._wg_interface],
            ]
            for cmd in cmds:
                proc = await asyncio.create_subprocess_exec(
                    *cmd, stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )
                await proc.wait()

            # Write private key to temp file (wg requires file, not stdin)
            import tempfile
            keyfile = tempfile.NamedTemporaryFile(mode="w", suffix=".key", delete=False)
            keyfile.write(privkey)
            keyfile.close()

            await (await asyncio.create_subprocess_exec(
                "wg", "set", self._wg_interface,
                "private-key", keyfile.name,
                "peer", relay_pubkey,
                "endpoint", self._relay_endpoint,
                "allowed-ips", allowed_ips,
                "persistent-keepalive", "25",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )).wait()

            os.unlink(keyfile.name)

            await (await asyncio.create_subprocess_exec(
                "ip", "link", "set", self._wg_interface, "up",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )).wait()

            log.info("Relay tunnel up: %s → %s (relay: %s)",
                     self._node_id, self._relay_ip, self._relay_endpoint)

        except Exception as e:
            log.warning("Relay tunnel setup failed: %s", e)
            self._wg_interface = ""

    async def _teardown_relay(self) -> None:
        """Remove the WireGuard relay interface."""
        if self._wg_interface:
            try:
                await (await asyncio.create_subprocess_exec(
                    "ip", "link", "del", self._wg_interface,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.DEVNULL,
                )).wait()
            except Exception:
                pass

    # ── Heartbeat ──────────────────────────────────────────────────────────

    async def _heartbeat_loop(self) -> None:
        """Send periodic heartbeats to Connect."""
        while True:
            await asyncio.sleep(60)
            await self._post("/nodes/heartbeat", {
                "node_id": self._node_id,
                "status": "online",
                "relay_ip": self._relay_ip,
                "uptime_s": round(time.monotonic(), 0),
            })

    # ── Metrics collection ───────────────────────────────────────────────

    async def _metrics_loop(self) -> None:
        """
        Collect connection metrics every 10 seconds.

        These are exposed via the node's HTTP API at /api/v1/connection
        so the controller can read them for dashboards, alarms, and
        adaptive quality decisions.
        """
        prev_packets = 0
        prev_time = time.monotonic()

        while True:
            await asyncio.sleep(10)
            now = time.monotonic()
            self.state.uptime_s = now

            # HID packet rate
            dt = now - prev_time
            if dt > 0:
                self.state.hid_packets_per_second = (
                    (self.state.hid_packets_received - prev_packets) / dt
                )
            prev_packets = self.state.hid_packets_received
            prev_time = now

            # Relay RTT (WireGuard ping)
            if self._relay_ip and self._wg_interface:
                self.state.relay_connected = True
                self.state.relay_ip = self._relay_ip
                rtt = await self._ping(self._relay_endpoint.split(":")[0])
                if rtt is not None:
                    self.state.relay_rtt_ms = rtt
            else:
                self.state.relay_connected = False

            # Connect API RTT
            if self.enabled:
                t0 = time.monotonic()
                result = await self._post("/health", {})
                if result:
                    self.state.connect_rtt_ms = (time.monotonic() - t0) * 1000
                    self.state.connect_reachable = True
                else:
                    self.state.connect_reachable = False

    async def _ping(self, host: str) -> float | None:
        """Measure RTT to a host via ICMP ping. Returns ms or None."""
        if not host:
            return None
        try:
            proc = await asyncio.create_subprocess_exec(
                "ping", "-c", "1", "-W", "2", host,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=3)
            # Parse "time=1.23 ms" from ping output
            output = stdout.decode()
            for part in output.split():
                if part.startswith("time="):
                    return float(part.split("=")[1])
        except Exception:
            pass
        return None

    # ── HTTP helpers ───────────────────────────────────────────────────────

    async def _post(self, path: str, body: dict) -> dict | None:
        import urllib.request
        try:
            loop = asyncio.get_running_loop()
            def _do():
                data = json.dumps(body).encode()
                req = urllib.request.Request(
                    f"{self._api_base}{path}",
                    data=data,
                    headers={
                        "Content-Type": "application/json",
                        "Authorization": f"Bearer {self._token}",
                    },
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=10) as r:
                    return json.loads(r.read())
            return await loop.run_in_executor(None, _do)
        except Exception:
            return None
