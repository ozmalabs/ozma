# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Cross-user resource sharing.

Handles sharing machines, services, audio outputs, and displays between
users on linked controllers.  Works on the local LAN (controller-to-
controller WireGuard mesh) and across the internet via Ozma Connect relay.

The same mechanism serves both housemates (LAN) and remote friends (relay).
Only the transport differs.

Persistence: ``shares.json`` next to main.py.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.sharing")


# ── Data models ──────────────────────────────────────────────────────────

@dataclass
class ShareGrant:
    """A grant giving one user access to another user's resource."""
    id: str
    grantor_user_id: str             # user sharing the resource
    grantee_user_id: str             # user receiving access
    resource_type: str               # "service" | "node" | "audio_output" | "display"
    resource_id: str                 # ServiceDefinition.id, node_id, etc.
    permissions: list[str] = field(default_factory=lambda: ["read"])
    alias: str = ""                  # "bobsjellyfin" (grantee's namespace)
    expires_at: float = 0.0          # 0 = no expiry
    created_at: float = 0.0
    revoked: bool = False

    @property
    def expired(self) -> bool:
        return self.expires_at > 0 and time.time() > self.expires_at

    @property
    def active(self) -> bool:
        return not self.revoked and not self.expired

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "grantor_user_id": self.grantor_user_id,
            "grantee_user_id": self.grantee_user_id,
            "resource_type": self.resource_type,
            "resource_id": self.resource_id,
            "permissions": self.permissions,
            "alias": self.alias,
            "expires_at": self.expires_at,
            "created_at": self.created_at,
            "revoked": self.revoked,
            "active": self.active,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ShareGrant:
        return cls(
            id=d["id"],
            grantor_user_id=d["grantor_user_id"],
            grantee_user_id=d["grantee_user_id"],
            resource_type=d.get("resource_type", "service"),
            resource_id=d.get("resource_id", ""),
            permissions=d.get("permissions", ["read"]),
            alias=d.get("alias", ""),
            expires_at=d.get("expires_at", 0.0),
            created_at=d.get("created_at", 0.0),
            revoked=d.get("revoked", False),
        )


@dataclass
class PeerController:
    """A linked controller belonging to another user."""
    id: str                          # controller identity (ed25519 pubkey fingerprint)
    owner_user_id: str
    name: str
    host: str                        # IP address (LAN or mesh overlay)
    port: int = 7380
    last_seen: float = 0.0
    online: bool = False
    transport: str = "lan"           # "lan" | "relay"

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "owner_user_id": self.owner_user_id,
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "last_seen": self.last_seen,
            "online": self.online,
            "transport": self.transport,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> PeerController:
        return cls(
            id=d["id"],
            owner_user_id=d.get("owner_user_id", ""),
            name=d.get("name", ""),
            host=d.get("host", ""),
            port=d.get("port", 7380),
            last_seen=d.get("last_seen", 0.0),
            online=d.get("online", False),
            transport=d.get("transport", "lan"),
        )


# ── Sharing manager ─────────────────────────────────────────────────────

class SharingManager:
    """Manages share grants and peer controller connections."""

    def __init__(self, path: Path) -> None:
        self._path = path
        self._grants: dict[str, ShareGrant] = {}
        self._peers: dict[str, PeerController] = {}
        self._health_task: asyncio.Task | None = None
        self._load()

    # ── Persistence ──────────────────────────────────────────────────

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text())
            for gd in data.get("grants", []):
                g = ShareGrant.from_dict(gd)
                self._grants[g.id] = g
            for pd in data.get("peers", []):
                p = PeerController.from_dict(pd)
                self._peers[p.id] = p
            log.info("Loaded %d grant(s), %d peer(s) from %s",
                     len(self._grants), len(self._peers), self._path.name)
        except Exception as e:
            log.warning("Failed to load shares: %s", e)

    def _save(self) -> None:
        data: dict[str, Any] = {
            "grants": [g.to_dict() for g in self._grants.values()],
            "peers": [p.to_dict() for p in self._peers.values()],
        }
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        tmp.replace(self._path)

    # ── Lifecycle ────────────────────────────────────────────────────

    async def start(self) -> None:
        self._health_task = asyncio.create_task(
            self._peer_health_monitor(), name="peer-health"
        )

    async def stop(self) -> None:
        if self._health_task:
            self._health_task.cancel()

    # ── Grant CRUD ───────────────────────────────────────────────────

    def create_grant(self, grantor_user_id: str, grantee_user_id: str,
                     resource_type: str, resource_id: str,
                     permissions: list[str] | None = None,
                     alias: str = "",
                     expires_at: float = 0.0) -> ShareGrant:
        grant = ShareGrant(
            id=str(uuid.uuid4()),
            grantor_user_id=grantor_user_id,
            grantee_user_id=grantee_user_id,
            resource_type=resource_type,
            resource_id=resource_id,
            permissions=permissions or ["read"],
            alias=alias,
            expires_at=expires_at,
            created_at=time.time(),
        )
        self._grants[grant.id] = grant
        self._save()
        log.info("Created share grant %s: %s → %s (%s/%s)",
                 grant.id, grantor_user_id, grantee_user_id,
                 resource_type, resource_id)
        return grant

    def revoke_grant(self, grant_id: str) -> bool:
        grant = self._grants.get(grant_id)
        if not grant:
            return False
        grant.revoked = True
        self._save()
        log.info("Revoked share grant %s", grant_id)
        return True

    def get_grant(self, grant_id: str) -> ShareGrant | None:
        return self._grants.get(grant_id)

    def list_grants_for_user(self, user_id: str) -> list[ShareGrant]:
        """Grants where user is the grantee (things shared WITH them)."""
        return [g for g in self._grants.values()
                if g.grantee_user_id == user_id and g.active]

    def list_grants_from_user(self, user_id: str) -> list[ShareGrant]:
        """Grants where user is the grantor (things they are SHARING)."""
        return [g for g in self._grants.values()
                if g.grantor_user_id == user_id and g.active]

    def list_all_grants(self) -> list[ShareGrant]:
        return list(self._grants.values())

    def find_grant_by_alias(self, alias: str, grantee_user_id: str) -> ShareGrant | None:
        """Find a grant by its alias in the grantee's namespace."""
        for g in self._grants.values():
            if g.alias == alias and g.grantee_user_id == grantee_user_id and g.active:
                return g
        return None

    # ── Peer management ──────────────────────────────────────────────

    def add_peer(self, controller_id: str, owner_user_id: str,
                 name: str, host: str, port: int = 7380,
                 transport: str = "lan") -> PeerController:
        peer = PeerController(
            id=controller_id,
            owner_user_id=owner_user_id,
            name=name,
            host=host,
            port=port,
            last_seen=time.time(),
            online=True,
            transport=transport,
        )
        self._peers[peer.id] = peer
        self._save()
        log.info("Added peer controller %s (%s) at %s:%d via %s",
                 name, controller_id, host, port, transport)
        return peer

    def remove_peer(self, controller_id: str) -> bool:
        peer = self._peers.pop(controller_id, None)
        if peer:
            self._save()
            log.info("Removed peer controller %s", controller_id)
            return True
        return False

    def get_peer(self, controller_id: str) -> PeerController | None:
        return self._peers.get(controller_id)

    def list_peers(self) -> list[PeerController]:
        return list(self._peers.values())

    def get_peer_for_user(self, user_id: str) -> PeerController | None:
        """Find the peer controller owned by a specific user."""
        for p in self._peers.values():
            if p.owner_user_id == user_id:
                return p
        return None

    # ── Cross-user proxy ─────────────────────────────────────────────

    async def proxy_to_shared_service(self, grant: ShareGrant,
                                       request: Any) -> Any:
        """Proxy a request to a shared service on a peer controller.

        Flow:
          1. Find the peer controller for the grantor
          2. Forward the request to peer's service proxy
          3. Return the response

        For Connect relay: the peer's host is a relay address, but the
        HTTP proxy flow is identical — the WireGuard tunnel handles
        the transport transparently.
        """
        peer = self.get_peer_for_user(grant.grantor_user_id)
        if not peer or not peer.online:
            from fastapi.responses import Response
            return Response(status_code=502, content="Peer controller unreachable")

        import httpx
        target_url = (f"http://{peer.host}:{peer.port}"
                      f"/api/v1/services/{grant.resource_id}/proxy")
        try:
            async with httpx.AsyncClient(timeout=30.0) as client:
                body = await request.body()
                resp = await client.request(
                    method=request.method,
                    url=target_url,
                    content=body,
                    headers={"X-Ozma-Grant-Id": grant.id},
                )
                from fastapi.responses import Response
                return Response(
                    content=resp.content,
                    status_code=resp.status_code,
                )
        except Exception as e:
            from fastapi.responses import Response
            return Response(status_code=502, content=f"Peer proxy error: {e}")

    # ── Peer health monitoring ───────────────────────────────────────

    async def _peer_health_monitor(self) -> None:
        """Ping peer controllers every 30 seconds."""
        import httpx
        while True:
            await asyncio.sleep(30)
            for peer in list(self._peers.values()):
                try:
                    async with httpx.AsyncClient(timeout=5.0) as client:
                        resp = await client.get(
                            f"http://{peer.host}:{peer.port}/health"
                        )
                        was_online = peer.online
                        peer.online = resp.status_code == 200
                        peer.last_seen = time.time()
                        if not was_online and peer.online:
                            log.info("Peer %s is now online", peer.name)
                        elif was_online and not peer.online:
                            log.info("Peer %s is now offline", peer.name)
                except Exception:
                    if peer.online:
                        log.info("Peer %s is now offline", peer.name)
                    peer.online = False
