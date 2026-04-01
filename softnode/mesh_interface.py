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
IP: assigned by controller from 10.200.X.0/24
Routes: 10.200.0.0/16 → ozma0 (all mesh traffic goes through the TAP)
DNS: mesh nodes resolvable by name (via controller as DNS proxy)
"""

from __future__ import annotations

import asyncio
import fcntl
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
                 gateway: str = "", mtu: int = 1400) -> None:
        self._mesh_ip = mesh_ip
        self._mesh_subnet = mesh_subnet
        self._gateway = gateway
        self._mtu = mtu
        self._tap_fd: int = -1
        self._active = False
        self._read_task: asyncio.Task | None = None
        self._on_packet: Any = None  # callback(packet: bytes) for mesh forwarding

    @property
    def active(self) -> bool:
        return self._active

    @property
    def interface_name(self) -> str:
        return INTERFACE_NAME

    @property
    def mesh_ip(self) -> str:
        return self._mesh_ip

    def set_packet_handler(self, handler: Any) -> None:
        """Set callback for packets read from the TAP (to be forwarded to mesh)."""
        self._on_packet = handler

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

        log.info("Mesh interface active: %s ip=%s subnet=%s",
                 INTERFACE_NAME, self._mesh_ip, self._mesh_subnet)
        return True

    async def stop(self) -> None:
        self._active = False
        if self._read_task:
            self._read_task.cancel()
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
        """Configure IP, MTU, and routes on the TAP interface."""
        try:
            cmds = [
                ["ip", "addr", "add", f"{self._mesh_ip}/24", "dev", INTERFACE_NAME],
                ["ip", "link", "set", INTERFACE_NAME, "mtu", str(self._mtu)],
                ["ip", "link", "set", INTERFACE_NAME, "up"],
                ["ip", "route", "add", self._mesh_subnet, "dev", INTERFACE_NAME],
            ]
            for cmd in cmds:
                result = subprocess.run(cmd, capture_output=True, text=True)
                if result.returncode != 0 and "File exists" not in result.stderr:
                    log.warning("Interface config failed: %s → %s",
                                " ".join(cmd), result.stderr.strip())

            log.debug("Interface configured: %s ip=%s mtu=%d",
                      INTERFACE_NAME, self._mesh_ip, self._mtu)
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

    def to_dict(self) -> dict:
        return {
            "interface": INTERFACE_NAME,
            "mesh_ip": self._mesh_ip,
            "mesh_subnet": self._mesh_subnet,
            "active": self._active,
            "mtu": self._mtu,
        }
