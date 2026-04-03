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

  If the claiming Desk goes offline, the Grid releases the claim and
  attempts failover: the highest-priority online Desk in the same
  failover group receives the claim automatically.

Feed sources (cascaded video):
  A Desk can declare itself a Feed (video/audio source) so other Desks
  can display its output as a PiP or background stream. Subscribers get
  the Feed's HLS/RTSP URL. When the Feed Desk goes offline, subscribers
  are notified via the "feed_offline" event.

V1.4: persistence, failover groups, feed sources, full API surface.
"""

from __future__ import annotations

import asyncio
import json
import logging
import socket
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

log = logging.getLogger("ozma.grid")

_HEALTH_INTERVAL = 5.0      # seconds between health checks
_DESK_STALE      = 30.0     # seconds until an unresponsive Desk is marked offline


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class DeskInfo:
    """A discovered Desk on the network."""
    id: str
    name: str
    host: str
    port: int
    marks: list[str] = field(default_factory=list)     # Mark IDs this Desk manages
    failover_group: str = ""    # Desks in the same group share failover
    priority: int = 0           # Higher = preferred failover target
    last_seen: float = field(default_factory=time.monotonic)
    online: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "marks": self.marks,
            "failover_group": self.failover_group,
            "priority": self.priority,
            "online": self.online,
            "last_seen": self.last_seen,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "DeskInfo":
        obj = cls(
            id=d["id"], name=d["name"],
            host=d["host"], port=d["port"],
        )
        for k in ("marks", "failover_group", "priority", "last_seen", "online"):
            if k in d:
                setattr(obj, k, d[k])
        return obj


@dataclass
class MarkClaim:
    """Tracks which Desk has active control of a Mark."""
    mark_id: str
    desk_id: str
    claimed_at: float = field(default_factory=time.monotonic)
    shared: bool = False    # True if multiple Desks can claim this Mark

    def to_dict(self) -> dict[str, Any]:
        return {
            "mark_id": self.mark_id,
            "desk_id": self.desk_id,
            "claimed_at": self.claimed_at,
            "shared": self.shared,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "MarkClaim":
        return cls(
            mark_id=d["mark_id"],
            desk_id=d["desk_id"],
            claimed_at=d.get("claimed_at", time.monotonic()),
            shared=d.get("shared", False),
        )


@dataclass
class FeedSource:
    """
    A Desk that publishes its video/audio output as a Feed.

    Other Desks can subscribe to receive the HLS or RTSP URL and
    display it as a PiP or background stream.
    """
    feed_id: str            # Unique feed identifier (usually desk_id + ":feed")
    desk_id: str            # Originating Desk
    name: str               # Human-readable name
    hls_url: str = ""       # HLS stream URL (for web UI / MJPEG players)
    rtsp_url: str = ""      # RTSP URL (for hardware decoders / VLC)
    audio: bool = False     # Whether this feed includes audio
    subscribers: list[str] = field(default_factory=list)  # Desk IDs subscribed
    online: bool = True

    def to_dict(self) -> dict[str, Any]:
        return {
            "feed_id":     self.feed_id,
            "desk_id":     self.desk_id,
            "name":        self.name,
            "hls_url":     self.hls_url,
            "rtsp_url":    self.rtsp_url,
            "audio":       self.audio,
            "subscribers": self.subscribers,
            "online":      self.online,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "FeedSource":
        obj = cls(feed_id=d["feed_id"], desk_id=d["desk_id"], name=d["name"])
        for k in ("hls_url", "rtsp_url", "audio", "subscribers", "online"):
            if k in d:
                setattr(obj, k, d[k])
        return obj


# ---------------------------------------------------------------------------
# GridService
# ---------------------------------------------------------------------------

class GridService:
    """
    Federation service for multi-Desk ozma installations.

    Announces itself via mDNS, discovers Desks, manages Mark claims,
    and provides health monitoring with automatic failover.
    """

    def __init__(
        self,
        name: str = "Ozma Grid",
        port: int = 7381,
        data_dir: Path | None = None,
    ) -> None:
        self._name = name
        self._port = port
        self._data_dir = data_dir or Path("/var/lib/ozma/grid")
        self._desks:  dict[str, DeskInfo]   = {}
        self._claims: dict[str, MarkClaim]  = {}   # mark_id → claim
        self._feeds:  dict[str, FeedSource] = {}   # feed_id → feed
        self._azc: AsyncZeroconf | None = None
        self._service_info: ServiceInfo | None = None
        self._tasks: list[asyncio.Task] = []
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        """Start Grid service: mDNS announcement + health monitoring."""
        self._data_dir.mkdir(parents=True, exist_ok=True)
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
        self._tasks.append(
            asyncio.create_task(self._health_loop(), name="grid-health")
        )
        log.info("Grid service started: %s @ %s:%d", self._name, local_ip, self._port)

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()
        if self._azc and self._service_info:
            try:
                await self._azc.async_unregister_service(self._service_info)
            except Exception:
                pass
            await self._azc.async_close()

    # ------------------------------------------------------------------
    # Desk management
    # ------------------------------------------------------------------

    def register_desk(self, desk: DeskInfo) -> None:
        self._desks[desk.id] = desk
        self._save()
        log.info("Desk registered: %s (%s:%d) group=%s prio=%d",
                 desk.name, desk.host, desk.port, desk.failover_group or "none", desk.priority)

    def update_desk(self, desk_id: str, **kwargs: Any) -> DeskInfo | None:
        desk = self._desks.get(desk_id)
        if not desk:
            return None
        for k, v in kwargs.items():
            if hasattr(desk, k):
                setattr(desk, k, v)
        self._save()
        return desk

    def unregister_desk(self, desk_id: str) -> None:
        self._desks.pop(desk_id, None)
        # Release all claims held by this Desk
        released = [mid for mid, c in self._claims.items() if c.desk_id == desk_id]
        for mark_id in released:
            del self._claims[mark_id]
        # Mark feeds from this Desk as offline
        for feed in self._feeds.values():
            if feed.desk_id == desk_id:
                feed.online = False
        self._save()

    def list_desks(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._desks.values()]

    def get_desk(self, desk_id: str) -> DeskInfo | None:
        return self._desks.get(desk_id)

    # ------------------------------------------------------------------
    # Mark claims
    # ------------------------------------------------------------------

    def claim_mark(self, mark_id: str, desk_id: str, shared: bool = False) -> bool:
        """
        Claim a Mark for a Desk.

        If the Mark is already claimed by another Desk, the claim is
        transferred. Returns True if the claim succeeded.
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
            shared=shared,
        )
        self._save()
        return True

    def release_mark(self, mark_id: str, desk_id: str) -> bool:
        """Release a Mark claim. Returns True if it was held by desk_id."""
        existing = self._claims.get(mark_id)
        if existing and existing.desk_id == desk_id:
            del self._claims[mark_id]
            self._save()
            return True
        return False

    def get_claim(self, mark_id: str) -> MarkClaim | None:
        return self._claims.get(mark_id)

    def list_claims(self) -> list[dict[str, Any]]:
        return [c.to_dict() for c in self._claims.values()]

    # ------------------------------------------------------------------
    # Failover
    # ------------------------------------------------------------------

    def failover_candidates(self, desk_id: str) -> list[DeskInfo]:
        """
        Return online Desks in the same failover group as desk_id,
        sorted by priority descending (best candidate first).

        Excludes desk_id itself.
        """
        desk = self._desks.get(desk_id)
        if not desk or not desk.failover_group:
            return []
        return sorted(
            [
                d for d in self._desks.values()
                if d.id != desk_id
                and d.online
                and d.failover_group == desk.failover_group
            ],
            key=lambda d: d.priority,
            reverse=True,
        )

    def _do_failover(self, failed_desk_id: str) -> list[str]:
        """
        Release all claims held by failed_desk_id and attempt to
        transfer them to the best failover candidate.

        Returns the list of mark_ids that were successfully transferred.
        """
        transferred: list[str] = []
        candidates = self.failover_candidates(failed_desk_id)
        for mark_id, claim in list(self._claims.items()):
            if claim.desk_id != failed_desk_id:
                continue
            if candidates:
                new_desk = candidates[0]
                self._claims[mark_id] = MarkClaim(
                    mark_id=mark_id,
                    desk_id=new_desk.id,
                    claimed_at=time.monotonic(),
                )
                log.info("Failover: Mark %s → Desk %s", mark_id, new_desk.id)
                transferred.append(mark_id)
            else:
                del self._claims[mark_id]
                log.info("Failover: Mark %s released (no candidates)", mark_id)
        if transferred or any(c.desk_id == failed_desk_id for c in self._claims.values()):
            self._save()
        return transferred

    # ------------------------------------------------------------------
    # Feed sources
    # ------------------------------------------------------------------

    def register_feed(self, feed: FeedSource) -> None:
        self._feeds[feed.feed_id] = feed
        self._save()
        log.info("Feed registered: %s from Desk %s", feed.feed_id, feed.desk_id)

    def unregister_feed(self, feed_id: str) -> bool:
        if feed_id not in self._feeds:
            return False
        del self._feeds[feed_id]
        self._save()
        return True

    def list_feeds(self) -> list[dict[str, Any]]:
        return [f.to_dict() for f in self._feeds.values()]

    def get_feed(self, feed_id: str) -> FeedSource | None:
        return self._feeds.get(feed_id)

    def subscribe_feed(self, feed_id: str, desk_id: str) -> bool:
        """Subscribe a Desk to a Feed. Returns False if feed not found."""
        feed = self._feeds.get(feed_id)
        if not feed:
            return False
        if desk_id not in feed.subscribers:
            feed.subscribers.append(desk_id)
            self._save()
        return True

    def unsubscribe_feed(self, feed_id: str, desk_id: str) -> bool:
        feed = self._feeds.get(feed_id)
        if not feed:
            return False
        if desk_id in feed.subscribers:
            feed.subscribers.remove(desk_id)
            self._save()
        return True

    # ------------------------------------------------------------------
    # Show state
    # ------------------------------------------------------------------

    def show_state(self) -> dict[str, Any]:
        """Return the full Show state (all Desks, claims, feeds)."""
        return {
            "grid_name": self._name,
            "desks":     self.list_desks(),
            "claims":    self.list_claims(),
            "feeds":     self.list_feeds(),
        }

    # ------------------------------------------------------------------
    # Health monitoring
    # ------------------------------------------------------------------

    async def _health_loop(self) -> None:
        """Monitor Desk health via REST API heartbeat, trigger failover."""
        while True:
            try:
                for desk in list(self._desks.values()):
                    alive = await self._check_desk_health(desk)
                    if not alive and desk.online:
                        desk.online = False
                        desk.last_seen = time.monotonic()
                        log.warning("Desk offline: %s — triggering failover", desk.name)
                        self._do_failover(desk.id)
                        # Mark feeds from this Desk as offline
                        for feed in self._feeds.values():
                            if feed.desk_id == desk.id:
                                feed.online = False
                    elif alive and not desk.online:
                        desk.online = True
                        desk.last_seen = time.monotonic()
                        log.info("Desk back online: %s", desk.name)
                        for feed in self._feeds.values():
                            if feed.desk_id == desk.id:
                                feed.online = True
                await asyncio.sleep(_HEALTH_INTERVAL)
            except asyncio.CancelledError:
                return
            except Exception:
                log.exception("Grid health loop error")
                await asyncio.sleep(_HEALTH_INTERVAL)

    async def _check_desk_health(self, desk: DeskInfo) -> bool:
        """Ping a Desk's API — returns True if the Desk responds."""
        import urllib.request
        try:
            loop = asyncio.get_running_loop()
            url = f"http://{desk.host}:{desk.port}/api/v1/status"
            await asyncio.wait_for(
                loop.run_in_executor(
                    None, lambda: urllib.request.urlopen(url, timeout=3)
                ),
                timeout=4.0,
            )
            return True
        except Exception:
            return False

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "grid_state.json"
        tmp = p.with_suffix(".tmp")
        data = {
            "desks":  {did: d.to_dict() for did, d in self._desks.items()},
            "claims": {mid: c.to_dict() for mid, c in self._claims.items()},
            "feeds":  {fid: f.to_dict() for fid, f in self._feeds.items()},
        }
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(p)

    def _load(self) -> None:
        p = self._data_dir / "grid_state.json"
        if not p.exists():
            return
        try:
            data = json.loads(p.read_text())
            for did, d in data.get("desks", {}).items():
                self._desks[did] = DeskInfo.from_dict(d)
            for mid, c in data.get("claims", {}).items():
                self._claims[mid] = MarkClaim.from_dict(c)
            for fid, f in data.get("feeds", {}).items():
                self._feeds[fid] = FeedSource.from_dict(f)
            log.debug("Grid loaded: %d desks, %d claims, %d feeds",
                      len(self._desks), len(self._claims), len(self._feeds))
        except Exception:
            log.exception("Failed to load grid state")

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

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
