# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Virtual mesh NIC for soft nodes — TUN/TAP interface to the ozma mesh.

Hardware nodes give target machines mesh network access via USB Ethernet
gadget. Soft nodes do the same via a virtual network interface (TUN/TAP).

The soft node creates a TAP device (ozma0) on the host, assigns a mesh
IP, and routes traffic between the TAP interface and the ozma mesh.
The host gets a new network interface connected to the mesh — alongside
whatever other networking it already has.

Result: every machine running ozma-softnode (or ozma-virtual-node) gets
a mesh IP and can reach every other machine in the mesh, regardless of
physical network topology.

Platform support:
  Linux:   TAP via /dev/net/tun (standard, no extra packages)
  macOS:   utun via system socket (or tuntaposx)
  Windows: Wintun or TAP-Windows (used by WireGuard/OpenVPN)

Interface name: ozma0
IPv4: assigned by controller from 10.200.X.0/24
IPv6: ULA address from fdXX:XXXX:XXXX::/48 (collision-proof fallback)
Routes: 10.200.0.0/16 + fdXX:XXXX:XXXX::/48 → ozma0
DNS: mesh nodes resolvable by name (via controller as DNS proxy)

If the IPv4 mesh subnet (10.200.0.0/16) collides with a local network
(e.g. laptop moved to an office using the same range), the IPv6 ULA
addresses still work — ULA space never conflicts with real networks.
"""

from __future__ import annotations

import asyncio
import fcntl
import ipaddress
import logging
import os
import struct
import subprocess
from typing import Any

log = logging.getLogger("ozma.softnode.mesh_interface")

# Linux TUN/TAP constants
IFF_TAP = 0x0002
IFF_NO_PI = 0x1000
TUNSETIFF = 0x400454ca

INTERFACE_NAME = "ozma0"


class MeshInterface:
    """
    Creates and manages a virtual network interface for the ozma mesh.

    On creation:
      1. Opens /dev/net/tun and creates a TAP device (ozma0)
      2. Assigns the mesh IP provided by the controller
      3. Sets up routes for the mesh subnet (10.200.0.0/16)
      4. Reads packets from the TAP and forwards to the mesh
      5. Receives mesh packets and writes to the TAP
    """

    def __init__(self, mesh_ip: str = "", mesh_subnet: str = "10.200.0.0/16",
                 mesh_ip6: str = "", mesh_subnet6: str = "",
                 gateway: str = "", mtu: int = 1400) -> None:
        self._mesh_ip = mesh_ip
        self._mesh_subnet = mesh_subnet
        self._mesh_ip6 = mesh_ip6        # ULA IPv6 address (collision-proof fallback)
        self._mesh_subnet6 = mesh_subnet6  # e.g. "fdXX:XXXX:XXXX::/48"
        self._gateway = gateway
        self._mtu = mtu
        self._tap_fd: int = -1
        self._active = False
        self._read_task: asyncio.Task | None = None
        self._conflict_task: asyncio.Task | None = None
        self._on_packet: Any = None  # callback(packet: bytes) for mesh forwarding
        self._on_conflict: Any = None  # callback() when IPv4 mesh collides with LAN

    @property
    def active(self) -> bool:
        return self._active

    @property
    def interface_name(self) -> str:
        return INTERFACE_NAME

    @property
    def mesh_ip(self) -> str:
        return self._mesh_ip

    @property
    def mesh_ip6(self) -> str:
        return self._mesh_ip6

    def set_packet_handler(self, handler: Any) -> None:
        """Set callback for packets read from the TAP (to be forwarded to mesh)."""
        self._on_packet = handler

    def set_conflict_handler(self, handler: Any) -> None:
        """Set callback for IPv4 mesh subnet conflicts with local routes."""
        self._on_conflict = handler

    async def start(self) -> bool:
        """Create the TAP interface and configure networking."""
        if not self._mesh_ip:
            log.warning("No mesh IP assigned — cannot start mesh interface")
            return False

        # Create TAP device
        if not await self._create_tap():
            return False

        # Configure the interface
        if not await self._configure_interface():
            await self.stop()
            return False

        # Start reading packets
        self._active = True
        self._read_task = asyncio.create_task(
            self._read_loop(), name="mesh-tap-read"
        )
        self._conflict_task = asyncio.create_task(
            self._monitor_route_conflicts(), name="mesh-conflict-monitor"
        )

        v6_info = f" v6={self._mesh_ip6}" if self._mesh_ip6 else ""
        log.info("Mesh interface active: %s ip=%s%s subnet=%s",
                 INTERFACE_NAME, self._mesh_ip, v6_info, self._mesh_subnet)
        return True

    async def stop(self) -> None:
        self._active = False
        if self._read_task:
            self._read_task.cancel()
        if self._conflict_task:
            self._conflict_task.cancel()
        if self._tap_fd >= 0:
            # Bring down the interface
            subprocess.run(["ip", "link", "set", INTERFACE_NAME, "down"],
                           capture_output=True)
            os.close(self._tap_fd)
            self._tap_fd = -1

    async def inject_packet(self, packet: bytes) -> None:
        """Write a packet into the TAP (received from the mesh)."""
        if self._tap_fd < 0:
            return
        try:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, os.write, self._tap_fd, packet)
        except OSError as e:
            log.debug("TAP write error: %s", e)

    # ── Internal ────────────────────────────────────────────────────────────

    async def _create_tap(self) -> bool:
        """Create a TAP device via /dev/net/tun."""
        try:
            tun_fd = os.open("/dev/net/tun", os.O_RDWR)

            # Configure as TAP (Ethernet frames, not IP packets)
            ifr = struct.pack("16sH", INTERFACE_NAME.encode(), IFF_TAP | IFF_NO_PI)
            fcntl.ioctl(tun_fd, TUNSETIFF, ifr)

            self._tap_fd = tun_fd
            log.debug("TAP device created: %s (fd=%d)", INTERFACE_NAME, tun_fd)
            return True
        except PermissionError:
            log.warning("Cannot create TAP device — need root or CAP_NET_ADMIN. "
                        "Run with: sudo ozma-softnode --name ... "
                        "Or: sudo setcap cap_net_admin+ep $(which python3)")
            return False
        except OSError as e:
            log.warning("TAP creation failed: %s", e)
            return False

    async def _configure_interface(self) -> bool:
        """Configure dual-stack IP, MTU, and routes on the TAP interface."""
        try:
            cmds = [
                ["ip", "addr", "add", f"{self._mesh_ip}/24", "dev", INTERFACE_NAME],
                ["ip", "link", "set", INTERFACE_NAME, "mtu", str(self._mtu)],
                ["ip", "link", "set", INTERFACE_NAME, "up"],
                ["ip", "route", "add", self._mesh_subnet, "dev", INTERFACE_NAME],
            ]
            # Add IPv6 ULA address and route if available
            if self._mesh_ip6 and self._mesh_subnet6:
                cmds.extend([
                    ["ip", "-6", "addr", "add", f"{self._mesh_ip6}/64",
                     "dev", INTERFACE_NAME],
                    ["ip", "-6", "route", "add", self._mesh_subnet6,
                     "dev", INTERFACE_NAME],
                ])
            for cmd in cmds:
                result = subprocess.run(cmd, capture_output=True, text=True)
                if result.returncode != 0 and "File exists" not in result.stderr:
                    log.warning("Interface config failed: %s → %s",
                                " ".join(cmd), result.stderr.strip())

            log.debug("Interface configured: %s ip=%s ip6=%s mtu=%d",
                      INTERFACE_NAME, self._mesh_ip,
                      self._mesh_ip6 or "(none)", self._mtu)
            return True
        except Exception as e:
            log.error("Interface configuration failed: %s", e)
            return False

    async def _read_loop(self) -> None:
        """Read Ethernet frames from the TAP and forward to mesh."""
        loop = asyncio.get_running_loop()

        while self._active:
            try:
                data = await loop.run_in_executor(
                    None, self._blocking_read
                )
                if data and self._on_packet:
                    await self._on_packet(data)
            except asyncio.CancelledError:
                return
            except Exception:
                if self._active:
                    await asyncio.sleep(0.1)

    def _blocking_read(self) -> bytes:
        """Blocking read from TAP fd with timeout."""
        import select as sel
        r, _, _ = sel.select([self._tap_fd], [], [], 1.0)
        if r:
            try:
                return os.read(self._tap_fd, 1518)  # max Ethernet frame
            except OSError:
                return b""
        return b""

    # ── IPv4 conflict detection ───────────────────────────────────────────

    async def detect_ipv4_conflict(self) -> bool:
        """Check if the mesh IPv4 subnet overlaps with any local route.

        Returns True if a conflict is found. IPv6 ULA mesh is unaffected
        by such conflicts — that's the whole point of dual-stack.
        """
        try:
            mesh_net = ipaddress.ip_network(self._mesh_subnet, strict=False)
        except ValueError:
            return False
        try:
            proc = await asyncio.create_subprocess_exec(
                "ip", "-4", "route", "show",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await proc.communicate()
            for line in stdout.decode().splitlines():
                parts = line.split()
                if not parts or parts[0] in ("default", "unreachable"):
                    continue
                if INTERFACE_NAME in line:
                    continue  # our own route
                try:
                    route_net = ipaddress.ip_network(parts[0], strict=False)
                    if route_net.overlaps(mesh_net):
                        log.warning(
                            "IPv4 mesh conflict: %s overlaps route '%s' — "
                            "IPv6 mesh (%s) still available",
                            self._mesh_subnet, line.strip(),
                            self._mesh_ip6 or "not configured",
                        )
                        return True
                except ValueError:
                    continue
        except Exception as e:
            log.debug("Route conflict check failed: %s", e)
        return False

    async def _monitor_route_conflicts(self, interval: float = 30.0) -> None:
        """Periodically check for IPv4 mesh subnet conflicts with local routes.

        Only fires the callback on state transitions (conflict detected /
        conflict resolved), not on every check while conflict persists.
        """
        was_conflicting = False
        while self._active:
            try:
                is_conflicting = await self.detect_ipv4_conflict()
                if is_conflicting and not was_conflicting:
                    if self._on_conflict:
                        await self._on_conflict()
                elif was_conflicting and not is_conflicting:
                    log.info("IPv4 mesh conflict resolved")
                was_conflicting = is_conflicting
            except asyncio.CancelledError:
                return
            except Exception:
                pass
            await asyncio.sleep(interval)

    def to_dict(self) -> dict:
        d = {
            "interface": INTERFACE_NAME,
            "mesh_ip": self._mesh_ip,
            "mesh_subnet": self._mesh_subnet,
            "active": self._active,
            "mtu": self._mtu,
        }
        if self._mesh_ip6:
            d["mesh_ip6"] = self._mesh_ip6
            d["mesh_subnet6"] = self._mesh_subnet6
        return d
