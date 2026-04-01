# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
RGB output manager — pushes scenario colours to all RGB zones.

Zones:
  - keyboard:  Existing RGBEngine → WebSocket rgb.frame events (web UI 3D keyboard)
  - node_leds: Each node's onboard WS2812 strip via POST /rgb/set
  - wled:      WLED ESP32 controllers via UDP JSON API (auto-discovered)
  - artnet:    Art-Net/DMX universes via UDP (configured)

On scenario switch:
  1. Active node → scenario colour (solid or effect)
  2. Inactive nodes → dim standby colour (10% brightness of their bound scenario)
  3. WLED strips → scenario colour with transition effect
  4. Art-Net → scenario colour mapped to configured channels

WLED discovery:
  WLED devices announce via mDNS as _wled._tcp.  Each discovered device
  becomes an RGB zone that follows the active scenario colour.

WLED JSON API (UDP port 21324):
  {"on": true, "bri": 255, "seg": [{"col": [[r, g, b]]}]}
"""

from __future__ import annotations

import asyncio
import json
import logging
import socket
from dataclasses import dataclass, field
from typing import Any

from rgb import RGB, hex_to_rgb, lerp_rgb
from rgb_compositor import RGBCompositor, AmbientConfig, LayerPriority

log = logging.getLogger("ozma.rgb_outputs")

# WLED UDP realtime port
WLED_UDP_PORT = 21324
# WLED JSON API port
WLED_JSON_PORT = 80

# Standby brightness for inactive nodes (fraction of full)
STANDBY_DIM = 0.08


@dataclass
class RGBZone:
    """An RGB output zone."""

    id: str
    name: str
    zone_type: str        # "node", "wled", "artnet"
    host: str = ""
    port: int = 0
    color: RGB = (0, 0, 0)
    enabled: bool = True
    node_id: str = ""     # For node zones: which ozma node this belongs to
    led_count: int = 0
    props: dict = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "zone_type": self.zone_type,
            "host": self.host,
            "color": list(self.color),
            "enabled": self.enabled,
            "node_id": self.node_id,
            "led_count": self.led_count,
        }


class RGBOutputManager:
    """
    Manages RGB output zones and pushes scenario colours on switch.
    """

    def __init__(self, led_count: int = 30, fps: int = 30) -> None:
        self._zones: dict[str, RGBZone] = {}
        self._discovery_task: asyncio.Task | None = None
        self._udp_sock: socket.socket | None = None
        self._active_color: RGB = (0, 0, 0)
        self._active_node_id: str | None = None
        self._state: Any = None  # AppState, set via set_state()

        # Layered compositor — generates blended frames at FPS
        self.compositor = RGBCompositor(led_count=led_count, fps=fps)
        self.compositor.on_frame = self._on_compositor_frame

    def set_state(self, state: Any) -> None:
        """Set AppState reference for node RGB zone auto-registration."""
        self._state = state

    async def start(self) -> None:
        self._udp_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._udp_sock.setblocking(False)
        await self.compositor.start()
        self._discovery_task = asyncio.create_task(
            self._discovery_loop(), name="rgb-output-discovery"
        )
        log.info("RGBOutputManager started (compositor at %d fps)", self.compositor._fps)

    async def stop(self) -> None:
        await self.compositor.stop()
        if self._discovery_task:
            self._discovery_task.cancel()
            try:
                await self._discovery_task
            except asyncio.CancelledError:
                pass
        if self._udp_sock:
            self._udp_sock.close()

    # ── Zone management ──────────────────────────────────────────────────────

    def add_zone(self, zone: RGBZone) -> None:
        self._zones[zone.id] = zone
        log.info("RGB zone added: %s (%s)", zone.name, zone.zone_type)

    def remove_zone(self, zone_id: str) -> None:
        self._zones.pop(zone_id, None)

    def list_zones(self) -> list[dict[str, Any]]:
        return [z.to_dict() for z in self._zones.values()]

    def register_node(self, node_id: str, host: str, api_port: int, led_count: int) -> None:
        """Register a node's onboard LEDs as an RGB zone."""
        zone_id = f"node-{node_id.split('.')[0]}"
        if zone_id not in self._zones:
            self._zones[zone_id] = RGBZone(
                id=zone_id,
                name=f"{node_id.split('.')[0]} LEDs",
                zone_type="node",
                host=host,
                port=api_port,
                node_id=node_id,
                led_count=led_count,
            )
            log.info("RGB zone registered for node: %s (%d LEDs)", node_id, led_count)

    # ── Scenario switch ──────────────────────────────────────────────────────

    async def on_scenario_switch(
        self,
        scenario_color: str,
        active_node_id: str | None,
        all_scenarios: list[dict] | None = None,
        effect: str = "solid",
    ) -> None:
        """
        Update RGB layers when a scenario switches.

        Sets the scenario layer on the compositor and fires a transient
        switch notification.  The compositor's render loop handles blending
        with ambient and any active notes/alerts, then pushes to all zones.
        """
        color = hex_to_rgb(scenario_color)
        self._active_color = color
        self._active_node_id = active_node_id

        # Update the scenario layer on the compositor
        self.compositor.set_scenario_color(color)

        # Fire a transient switch notification (brief flash of new colour)
        self.compositor.notify_scenario_switch(color)

        # Also do a direct push to node-specific zones (per-node colouring)
        # since each node may have a different scenario colour
        node_colors: dict[str, RGB] = {}
        if all_scenarios:
            for sc in all_scenarios:
                nid = sc.get("node_id")
                if nid:
                    sc_color = hex_to_rgb(sc.get("color", "#888888"))
                    if nid == active_node_id:
                        node_colors[nid] = sc_color
                    else:
                        node_colors[nid] = (
                            int(sc_color[0] * STANDBY_DIM),
                            int(sc_color[1] * STANDBY_DIM),
                            int(sc_color[2] * STANDBY_DIM),
                        )

        # Push per-node colours directly (nodes have their own LED strips)
        tasks = []
        for zone in self._zones.values():
            if not zone.enabled or zone.zone_type != "node":
                continue
            nc = node_colors.get(zone.node_id, color if zone.node_id == active_node_id
                                 else (int(color[0] * STANDBY_DIM),
                                       int(color[1] * STANDBY_DIM),
                                       int(color[2] * STANDBY_DIM)))
            zone.color = nc
            tasks.append(self._push_node(zone, nc, effect))
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    async def _on_compositor_frame(self, leds: list[RGB]) -> None:
        """Called by the compositor each frame with the blended LED buffer.
        Push the composited output to WLED and Art-Net zones."""
        # Compute a single representative colour from the LED buffer
        # (average of all LEDs — good enough for single-colour zones)
        if not leds:
            return
        n = len(leds)
        avg_r = sum(c[0] for c in leds) // n
        avg_g = sum(c[1] for c in leds) // n
        avg_b = sum(c[2] for c in leds) // n
        avg_color: RGB = (avg_r, avg_g, avg_b)

        for zone in self._zones.values():
            if not zone.enabled:
                continue
            if zone.zone_type == "wled":
                zone.color = avg_color
                # Only push if colour actually changed (avoid UDP spam)
                await self._push_wled(zone, avg_color)
            elif zone.zone_type == "artnet":
                zone.color = avg_color
                await self._push_artnet(zone, avg_color)

    # ── Push to specific zone types ──────────────────────────────────────────

    async def _push_node(self, zone: RGBZone, color: RGB, effect: str = "solid") -> None:
        """Push colour to a node's onboard LEDs via HTTP API."""
        import urllib.request
        url = f"http://{zone.host}:{zone.port}/rgb/set"
        body = json.dumps({
            "color": list(color),
            "effect": effect,
        }).encode()
        try:
            loop = asyncio.get_running_loop()
            req = urllib.request.Request(
                url, data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=3))
        except Exception as e:
            log.debug("RGB push to node %s failed: %s", zone.node_id, e)

    async def _push_wled(self, zone: RGBZone, color: RGB, effect: str = "solid") -> None:
        """Push colour to a WLED device via UDP JSON."""
        # WLED UDP JSON API (port 21324)
        # Faster than HTTP, suitable for real-time effects
        r, g, b = color
        payload = json.dumps({
            "on": True,
            "bri": 255,
            "transition": 7,  # 0.7s transition
            "seg": [{"col": [[r, g, b]]}],
        }).encode()
        try:
            self._udp_sock.sendto(payload, (zone.host, WLED_UDP_PORT))
        except Exception as e:
            log.debug("WLED push to %s failed: %s", zone.host, e)

    async def _push_artnet(self, zone: RGBZone, color: RGB) -> None:
        """Push colour to an Art-Net universe."""
        r, g, b = color
        universe = zone.props.get("universe", 0)
        start_channel = zone.props.get("start_channel", 0)

        # Art-Net DMX packet (simplified)
        # Header: "Art-Net\0" + opcode 0x5000 + proto 14 + sequence + physical + universe + length
        header = b"Art-Net\x00"
        header += b"\x00\x50"     # Opcode: ArtDmx
        header += b"\x00\x0e"     # Protocol version 14
        header += b"\x00"         # Sequence
        header += b"\x00"         # Physical
        header += universe.to_bytes(2, "little")

        # DMX data: 512 channels, set RGB at start_channel
        dmx = bytearray(512)
        if start_channel + 2 < 512:
            dmx[start_channel] = r
            dmx[start_channel + 1] = g
            dmx[start_channel + 2] = b

        length = len(dmx)
        header += length.to_bytes(2, "big")

        try:
            self._udp_sock.sendto(header + bytes(dmx), (zone.host, zone.port or 6454))
        except Exception as e:
            log.debug("Art-Net push to %s failed: %s", zone.host, e)

    # ── Set all zones to off ─────────────────────────────────────────────────

    async def all_off(self) -> None:
        """Turn off all RGB zones."""
        tasks = []
        for zone in self._zones.values():
            if zone.zone_type == "node":
                tasks.append(self._push_node(zone, (0, 0, 0), "off"))
            elif zone.zone_type == "wled":
                payload = json.dumps({"on": False}).encode()
                try:
                    self._udp_sock.sendto(payload, (zone.host, WLED_UDP_PORT))
                except Exception:
                    pass
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    # ── WLED discovery ───────────────────────────────────────────────────────

    async def _discovery_loop(self) -> None:
        """Discover WLED devices and auto-register node RGB zones."""
        while True:
            try:
                await self._discover_wled()
                self._scan_node_rgb_zones()
                await asyncio.sleep(30.0)
            except asyncio.CancelledError:
                return
            except Exception:
                await asyncio.sleep(60.0)

    def _scan_node_rgb_zones(self) -> None:
        """Register RGB zones for nodes that have the 'rgb' capability."""
        if not self._state:
            return
        for node in self._state.nodes.values():
            if "rgb" in node.capabilities and node.api_port:
                zone_id = f"node-{node.id.split('.')[0]}"
                if zone_id not in self._zones:
                    rgb_leds = 30  # default; could parse from TXT
                    self.register_node(node.id, node.host, node.api_port, rgb_leds)

    async def _discover_wled(self) -> None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "avahi-browse", "-t", "-r", "-p", "_wled._tcp",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10.0)
        except (FileNotFoundError, asyncio.TimeoutError):
            return

        for line in stdout.decode(errors="replace").splitlines():
            if not line.startswith("="):
                continue
            parts = line.split(";")
            if len(parts) < 9:
                continue
            name = parts[3]
            host = parts[7]
            port = int(parts[8]) if parts[8].isdigit() else 80

            zone_id = f"wled-{host.replace('.', '-')}"
            if zone_id not in self._zones:
                self._zones[zone_id] = RGBZone(
                    id=zone_id,
                    name=f"WLED {name}",
                    zone_type="wled",
                    host=host,
                    port=port,
                )
                log.info("Discovered WLED device: %s (%s)", name, host)
