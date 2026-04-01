# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Grid service — multi-Desk federation for ozma.

The Grid connects multiple Desks (operator positions) on the same
network, enabling shared Mark (node) access, cascaded video (Feeds),
and redundant failover.

Terminology:
  Desk  — an operator position (display + input + audio + scenarios)
  Mark  — a device attached to a target machine (formerly "node")
  Grid  — this service: federation, claim arbitration, health
  Feed  — a Desk whose output is a source for another Desk
  Show  — all Desks and Marks managed by a Grid

mDNS:
  Desks announce: _ozma-desk._tcp.local.
  Grid announces:  _ozma-grid._tcp.local.

Mark claims:
  Only one Desk sends HID to a Mark at a time.  When Desk B activates
  a scenario using Mark X (currently claimed by Desk A), the Grid:
    1. Notifies Desk A that Mark X is being released
    2. Desk A disconnects HID + audio
    3. Grid transfers the claim to Desk B
    4. Desk B connects HID + audio

This is the V1.4 prototype — basic Desk discovery, Grid registry,
and Mark claim management.
"""

from __future__ import annotations

import asyncio
import json
import logging
import socket
import time
from dataclasses import dataclass, field
from typing import Any

from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

log = logging.getLogger("ozma.grid")


@dataclass
class DeskInfo:
    """A discovered Desk on the network."""
    id: str
    name: str
    host: str
    port: int
    marks: list[str] = field(default_factory=list)  # Mark IDs this Desk manages
    last_seen: float = 0.0
    online: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "host": self.host,
            "port": self.port, "marks": self.marks, "online": self.online,
        }


@dataclass
class MarkClaim:
    """Tracks which Desk has active control of a Mark."""
    mark_id: str
    desk_id: str
    claimed_at: float = 0.0
    shared: bool = False  # True if multiple Desks can claim this Mark

    def to_dict(self) -> dict[str, Any]:
        return {"mark_id": self.mark_id, "desk_id": self.desk_id, "shared": self.shared}


class GridService:
    """
    Federation service for multi-Desk ozma installations.

    Announces itself via mDNS, discovers Desks, manages Mark claims,
    and provides health monitoring for failover.
    """

    def __init__(self, name: str = "Ozma Grid", port: int = 7381) -> None:
        self._name = name
        self._port = port
        self._desks: dict[str, DeskInfo] = {}
        self._claims: dict[str, MarkClaim] = {}  # mark_id → claim
        self._azc: AsyncZeroconf | None = None
        self._service_info: ServiceInfo | None = None
        self._health_task: asyncio.Task | None = None

    async def start(self) -> None:
        """Start Grid service: mDNS announcement + Desk discovery."""
        # Announce ourselves
        local_ip = self._local_ip()
        self._service_info = ServiceInfo(
            "_ozma-grid._tcp.local.",
            f"{self._name}._ozma-grid._tcp.local.",
            addresses=[socket.inet_aton(local_ip)],
            port=self._port,
            properties={"proto": "1", "name": self._name},
        )
        self._azc = AsyncZeroconf()
        await self._azc.async_register_service(self._service_info)

        self._health_task = asyncio.create_task(self._health_loop(), name="grid-health")
        log.info("Grid service started: %s @ %s:%d", self._name, local_ip, self._port)

    async def stop(self) -> None:
        if self._health_task:
            self._health_task.cancel()
        if self._azc and self._service_info:
            await self._azc.async_unregister_service(self._service_info)
            await self._azc.async_close()

    # ── Desk management ──────────────────────────────────────────────────────

    def register_desk(self, desk: DeskInfo) -> None:
        self._desks[desk.id] = desk
        log.info("Desk registered: %s (%s:%d)", desk.name, desk.host, desk.port)

    def unregister_desk(self, desk_id: str) -> None:
        self._desks.pop(desk_id, None)
        # Release all claims held by this Desk
        for mark_id in list(self._claims):
            if self._claims[mark_id].desk_id == desk_id:
                del self._claims[mark_id]

    def list_desks(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._desks.values()]

    # ── Mark claims ──────────────────────────────────────────────────────────

    def claim_mark(self, mark_id: str, desk_id: str) -> bool:
        """
        Claim a Mark for a Desk.

        If the Mark is already claimed by another Desk, the claim is
        transferred (the previous Desk loses control).

        Returns True if the claim succeeded.
        """
        existing = self._claims.get(mark_id)
        if existing and existing.desk_id == desk_id:
            return True  # Already claimed by this Desk

        if existing:
            log.info("Mark %s: claim transferred %s → %s",
                     mark_id, existing.desk_id, desk_id)

        self._claims[mark_id] = MarkClaim(
            mark_id=mark_id,
            desk_id=desk_id,
            claimed_at=time.monotonic(),
        )
        return True

    def release_mark(self, mark_id: str, desk_id: str) -> bool:
        """Release a Mark claim."""
        existing = self._claims.get(mark_id)
        if existing and existing.desk_id == desk_id:
            del self._claims[mark_id]
            return True
        return False

    def get_claim(self, mark_id: str) -> MarkClaim | None:
        return self._claims.get(mark_id)

    def list_claims(self) -> list[dict[str, Any]]:
        return [c.to_dict() for c in self._claims.values()]

    # ── Show state ───────────────────────────────────────────────────────────

    def show_state(self) -> dict[str, Any]:
        """Return the full Show state (all Desks + claims)."""
        return {
            "grid_name": self._name,
            "desks": self.list_desks(),
            "claims": self.list_claims(),
        }

    # ── Health monitoring ────────────────────────────────────────────────────

    async def _health_loop(self) -> None:
        """Monitor Desk health via REST API heartbeat."""
        while True:
            try:
                for desk in list(self._desks.values()):
                    alive = await self._check_desk_health(desk)
                    if not alive and desk.online:
                        desk.online = False
                        log.warning("Desk offline: %s", desk.name)
                        # Release all claims (trigger failover)
                        for mark_id in list(self._claims):
                            if self._claims[mark_id].desk_id == desk.id:
                                del self._claims[mark_id]
                    elif alive and not desk.online:
                        desk.online = True
                        desk.last_seen = time.monotonic()
                        log.info("Desk back online: %s", desk.name)

                await asyncio.sleep(5.0)
            except asyncio.CancelledError:
                return

    async def _check_desk_health(self, desk: DeskInfo) -> bool:
        """Ping a Desk's API."""
        import urllib.request
        try:
            loop = asyncio.get_running_loop()
            url = f"http://{desk.host}:{desk.port}/api/v1/status"
            await loop.run_in_executor(
                None, lambda: urllib.request.urlopen(url, timeout=3)
            )
            return True
        except Exception:
            return False

    @staticmethod
    def _local_ip() -> str:
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            s.connect(("8.8.8.8", 80))
            ip = s.getsockname()[0]
            s.close()
            return ip
        except OSError:
            return "127.0.0.1"
