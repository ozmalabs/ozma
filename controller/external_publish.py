# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
External service publishing.

Publish internal services to the internet under ``.e.`` subdomains
(e.g. ``jellyfin.alice.e.ozma.dev``).

Two modes:
  - **private**: authenticated via the user's IdP — access your own
    services from the internet securely.
  - **public**: open to anyone (with confirmation + dashboard warning).
    For blogs, public Jellyfin instances, etc.

This is essentially Cloudflare Tunnel built into the stack.  Requires
Ozma Connect for DNS + relay infrastructure.  Potential ~$5/month addon.

Persistence: ``publish.json`` next to main.py.
"""

from __future__ import annotations

import json
import logging
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.external_publish")


@dataclass
class ExternalPublish:
    """An externally published service."""
    id: str
    service_id: str                  # → ServiceDefinition.id
    owner_user_id: str
    mode: str = "private"            # "private" | "public"
    external_subdomain: str = ""     # "jellyfin" → jellyfin.alice.e.ozma.dev
    rate_limit: int = 0              # requests/min (0 = unlimited)
    allowed_domains: list[str] = field(default_factory=list)  # email domain allowlist
    enabled: bool = True
    created_at: float = 0.0
    # Set by Connect after provisioning
    external_domain: str = ""        # full domain: "jellyfin.alice.e.ozma.dev"
    provisioned: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "service_id": self.service_id,
            "owner_user_id": self.owner_user_id,
            "mode": self.mode,
            "external_subdomain": self.external_subdomain,
            "external_domain": self.external_domain,
            "rate_limit": self.rate_limit,
            "allowed_domains": self.allowed_domains,
            "enabled": self.enabled,
            "created_at": self.created_at,
            "provisioned": self.provisioned,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ExternalPublish:
        return cls(
            id=d["id"],
            service_id=d.get("service_id", ""),
            owner_user_id=d.get("owner_user_id", ""),
            mode=d.get("mode", "private"),
            external_subdomain=d.get("external_subdomain", ""),
            external_domain=d.get("external_domain", ""),
            rate_limit=d.get("rate_limit", 0),
            allowed_domains=d.get("allowed_domains", []),
            enabled=d.get("enabled", True),
            created_at=d.get("created_at", 0.0),
            provisioned=d.get("provisioned", False),
        )


class ExternalPublishManager:
    """Manages externally published services.

    Coordinates with Ozma Connect for DNS provisioning and relay setup.
    The controller handles SSL termination and IdP gating locally.
    """

    def __init__(self, path: Path) -> None:
        self._path = path
        self._entries: dict[str, ExternalPublish] = {}
        self._load()

    # ── Persistence ──────────────────────────────────────────────────

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text())
            for ed in data.get("entries", []):
                e = ExternalPublish.from_dict(ed)
                self._entries[e.id] = e
            log.info("Loaded %d published service(s) from %s",
                     len(self._entries), self._path.name)
        except Exception as e:
            log.warning("Failed to load publish entries: %s", e)

    def _save(self) -> None:
        data: dict[str, Any] = {
            "entries": [e.to_dict() for e in self._entries.values()],
        }
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        tmp.replace(self._path)

    # ── CRUD ─────────────────────────────────────────────────────────

    async def publish(self, service_id: str, owner_user_id: str,
                      external_subdomain: str, mode: str = "private",
                      rate_limit: int = 0,
                      allowed_domains: list[str] | None = None,
                      connect_client: Any = None,
                      username: str = "") -> ExternalPublish:
        """Publish a service externally.

        If Connect is available, provisions the ``.e.`` subdomain.
        """
        if mode not in ("private", "public"):
            raise ValueError(f"Invalid mode: {mode}")
        if mode == "public":
            log.warning("Publishing service %s as PUBLIC — accessible by anyone",
                        service_id)

        entry = ExternalPublish(
            id=str(uuid.uuid4()),
            service_id=service_id,
            owner_user_id=owner_user_id,
            mode=mode,
            external_subdomain=external_subdomain,
            rate_limit=rate_limit,
            allowed_domains=allowed_domains or [],
            created_at=time.time(),
        )

        # Provision external subdomain via Connect
        if connect_client and username:
            domain = await connect_client.provision_external_subdomain(
                external_subdomain, username,
            )
            if domain:
                entry.external_domain = domain
                entry.provisioned = True
                log.info("Provisioned external domain: %s", domain)
            else:
                log.warning("Failed to provision external subdomain — "
                            "service published locally only")

        self._entries[entry.id] = entry
        self._save()
        log.info("Published service %s externally as %s (mode=%s)",
                 service_id, external_subdomain, mode)
        return entry

    async def unpublish(self, entry_id: str) -> bool:
        entry = self._entries.pop(entry_id, None)
        if not entry:
            return False
        # TODO: notify Connect to remove DNS record
        self._save()
        log.info("Unpublished service %s", entry.service_id)
        return True

    def get_entry(self, entry_id: str) -> ExternalPublish | None:
        return self._entries.get(entry_id)

    def list_entries(self) -> list[ExternalPublish]:
        return list(self._entries.values())

    def update_entry(self, entry_id: str, **kwargs: Any) -> ExternalPublish | None:
        entry = self._entries.get(entry_id)
        if not entry:
            return None
        for key in ("mode", "rate_limit", "allowed_domains", "enabled"):
            if key in kwargs:
                setattr(entry, key, kwargs[key])
        if "mode" in kwargs and kwargs["mode"] == "public":
            log.warning("Service %s changed to PUBLIC mode", entry.service_id)
        self._save()
        return entry

    def find_by_domain(self, host: str) -> ExternalPublish | None:
        """Match a Host header to a published service."""
        host = host.split(":")[0].lower()
        for entry in self._entries.values():
            if entry.enabled and entry.external_domain and host == entry.external_domain:
                return entry
        return None
