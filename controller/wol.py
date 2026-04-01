# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Wake-on-LAN — power on machines via magic packet.

Sends the standard WoL magic packet (6× 0xFF + 16× target MAC) to the
broadcast address.  Works alongside the GPIO power relay — WoL for
machines that support it, relay for machines that don't.

MAC addresses are learned from:
  1. Node mDNS announcement (if node includes mac= in TXT)
  2. ARP table (controller has talked to the node before)
  3. Manual configuration in scenarios.json

Usage:
  POST /api/v1/nodes/{id}/wol   — wake a specific node's target machine
"""

from __future__ import annotations

import asyncio
import logging
import re
import socket
import subprocess
from typing import Any

log = logging.getLogger("ozma.wol")


def send_wol(mac: str, broadcast: str = "255.255.255.255", port: int = 9) -> bool:
    """Send a Wake-on-LAN magic packet."""
    mac_clean = mac.replace(":", "").replace("-", "").lower()
    if len(mac_clean) != 12:
        log.warning("Invalid MAC for WoL: %s", mac)
        return False

    mac_bytes = bytes.fromhex(mac_clean)
    magic = b"\xff" * 6 + mac_bytes * 16

    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        sock.sendto(magic, (broadcast, port))
        sock.close()
        log.info("WoL sent to %s via %s:%d", mac, broadcast, port)
        return True
    except OSError as e:
        log.warning("WoL failed for %s: %s", mac, e)
        return False


def get_mac_from_arp(ip: str) -> str | None:
    """Look up a MAC address from the system ARP table."""
    try:
        result = subprocess.run(["arp", "-n", ip], capture_output=True, text=True, timeout=3)
        for line in result.stdout.splitlines():
            m = re.search(r"([0-9a-fA-F]{2}(?::[0-9a-fA-F]{2}){5})", line)
            if m:
                return m.group(1)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    # Try ip neigh
    try:
        result = subprocess.run(["ip", "neigh", "show", ip], capture_output=True, text=True, timeout=3)
        m = re.search(r"([0-9a-fA-F]{2}(?::[0-9a-fA-F]{2}){5})", result.stdout)
        if m:
            return m.group(1)
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None
