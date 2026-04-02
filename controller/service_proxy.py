# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Service proxy — reverse proxy for internal and shared services.

Registers internal services (Jellyfin, Gitea, etc.) and proxies requests
based on the ``Host`` header.  Each service gets a subdomain under the
user's Connect domain (e.g. ``jellyfin.alice.c.ozma.dev``).

Works without Connect in HTTP-only mode (subdomain matching against
``*.localhost`` or custom domain).  With Connect, obtains a wildcard
Let's Encrypt certificate via DNS-01 challenge.

Persistence: ``services.json`` next to main.py.
"""

from __future__ import annotations

import asyncio
import ipaddress
import json
import logging
import re
import time
import uuid
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import httpx
from fastapi import Request
from fastapi.responses import Response, StreamingResponse

# ── Security: target host validation ─────────────────────────────────────

# Blocked IP ranges — prevent SSRF to internal infrastructure
_BLOCKED_NETWORKS = [
    ipaddress.ip_network("0.0.0.0/8"),          # "this" network
    ipaddress.ip_network("100.64.0.0/10"),       # carrier-grade NAT
    ipaddress.ip_network("169.254.0.0/16"),      # link-local (cloud metadata!)
    ipaddress.ip_network("224.0.0.0/4"),         # multicast
    ipaddress.ip_network("240.0.0.0/4"),         # reserved
    ipaddress.ip_network("255.255.255.255/32"),  # broadcast
    ipaddress.ip_network("::1/128"),             # IPv6 loopback
    ipaddress.ip_network("fe80::/10"),           # IPv6 link-local
    ipaddress.ip_network("fc00::/7"),            # IPv6 ULA
]

# Subdomain regex: lowercase alphanumeric + hyphens, 1-63 chars, no leading/trailing hyphen
_SUBDOMAIN_RE = re.compile(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$")

# Reserved subdomains that must not be registered
_RESERVED_SUBDOMAINS = frozenset({
    "api", "auth", "admin", "www", "static", "health", "docs",
    "connect", "relay", "internal", "localhost", "mail", "ftp",
})


def validate_target_host(host: str) -> None:
    """Validate that a service target host is safe to proxy to.

    Raises ValueError if the host points at a blocked network.
    Allows private RFC1918 ranges (10.x, 172.16-31.x, 192.168.x)
    because those are legitimate LAN services.
    """
    try:
        addr = ipaddress.ip_address(host)
    except ValueError:
        # It's a hostname — resolve it and check the result
        # For now, allow hostnames but block obvious dangerous ones
        if host.lower() in ("metadata.google.internal", "metadata", "instance-data"):
            raise ValueError(f"Blocked target host: {host}")
        return

    for net in _BLOCKED_NETWORKS:
        if addr in net:
            raise ValueError(
                f"Blocked target host {host} — "
                f"falls within reserved network {net}"
            )


def validate_subdomain(subdomain: str) -> None:
    """Validate a subdomain label. Raises ValueError if invalid."""
    if not _SUBDOMAIN_RE.match(subdomain):
        raise ValueError(
            f"Invalid subdomain '{subdomain}' — must be 1-63 lowercase "
            f"alphanumeric characters or hyphens, no leading/trailing hyphen"
        )
    if subdomain in _RESERVED_SUBDOMAINS:
        raise ValueError(f"Subdomain '{subdomain}' is reserved")

log = logging.getLogger("ozma.service_proxy")


# ── Data model ───────────────────────────────────────────────────────────

@dataclass
class ServiceDefinition:
    id: str
    name: str                        # Human label: "Jellyfin"
    owner_user_id: str               # Who registered this service
    target_host: str = "127.0.0.1"   # IP/hostname of the actual service
    target_port: int = 0             # Port of the actual service
    protocol: str = "http"           # http | https | tcp
    subdomain: str = ""              # "jellyfin" → jellyfin.alice.c.ozma.dev
    service_type: str = ""           # "jellyfin" | "immich" | "gitea" — for plugin matching
    auth_required: bool = True       # Gate behind IdP session
    health_path: str = "/health"     # Health check endpoint
    icon: str = ""                   # Icon URL for dashboard
    enabled: bool = True
    created_at: float = 0.0

    # Runtime state (not persisted)
    healthy: bool = True
    last_health_check: float = 0.0

    def target_url(self) -> str:
        return f"{self.protocol}://{self.target_host}:{self.target_port}"

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "owner_user_id": self.owner_user_id,
            "target_host": self.target_host,
            "target_port": self.target_port,
            "protocol": self.protocol,
            "subdomain": self.subdomain,
            "service_type": self.service_type,
            "auth_required": self.auth_required,
            "health_path": self.health_path,
            "icon": self.icon,
            "enabled": self.enabled,
            "created_at": self.created_at,
            "healthy": self.healthy,
        }

    def to_storage(self) -> dict[str, Any]:
        """Persist — excludes runtime state."""
        d = self.to_dict()
        d.pop("healthy", None)
        return d

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> ServiceDefinition:
        return cls(
            id=d["id"],
            name=d["name"],
            owner_user_id=d.get("owner_user_id", ""),
            target_host=d.get("target_host", "127.0.0.1"),
            target_port=int(d.get("target_port", 0)),
            protocol=d.get("protocol", "http"),
            subdomain=d.get("subdomain", ""),
            service_type=d.get("service_type", ""),
            auth_required=d.get("auth_required", True),
            health_path=d.get("health_path", "/health"),
            icon=d.get("icon", ""),
            enabled=d.get("enabled", True),
            created_at=d.get("created_at", 0.0),
        )


# ── Certificate management ───────────────────────────────────────────────

@dataclass
class CertState:
    """Tracks the wildcard certificate for a user's Connect subdomain."""
    domain: str = ""                 # e.g. "*.alice.c.ozma.dev"
    cert_path: str = ""              # path to PEM cert file
    key_path: str = ""               # path to PEM private key file
    expires_at: float = 0.0
    provisioned: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "domain": self.domain,
            "cert_path": self.cert_path,
            "key_path": self.key_path,
            "expires_at": self.expires_at,
            "provisioned": self.provisioned,
        }


class CertManager:
    """ACME wildcard certificate management.

    DNS-01 challenge coordinated via Ozma Connect:
      1. Controller generates private key locally
      2. Controller sends CSR to Connect
      3. Connect sets _acme-challenge TXT record in ozma.dev zone
      4. Controller completes ACME exchange with Let's Encrypt
      5. Private key NEVER leaves the controller
    """

    def __init__(self, certs_dir: Path) -> None:
        self._certs_dir = certs_dir
        self._certs_dir.mkdir(parents=True, exist_ok=True)
        self.state = CertState()

    async def provision_wildcard(self, domain: str, connect_client: Any) -> bool:
        """Provision a wildcard cert for ``*.domain`` via DNS-01 + Connect."""
        # TODO: implement ACME client with DNS-01 challenge via Connect API
        log.info("Certificate provisioning for %s — not yet implemented", domain)
        return False

    async def renew_if_needed(self) -> bool:
        """Check expiry and renew if within 30 days."""
        if not self.state.provisioned:
            return False
        days_left = (self.state.expires_at - time.time()) / 86400
        if days_left > 30:
            return False
        log.info("Certificate expires in %.0f days, renewing...", days_left)
        # TODO: implement renewal
        return False

    def has_valid_cert(self) -> bool:
        return (
            self.state.provisioned
            and self.state.cert_path
            and self.state.key_path
            and Path(self.state.cert_path).exists()
            and Path(self.state.key_path).exists()
            and self.state.expires_at > time.time()
        )


# ── Service proxy manager ────────────────────────────────────────────────

class ServiceProxyManager:
    """Manages registered services and proxies HTTP requests to them."""

    def __init__(self, path: Path, user_domain: str = "") -> None:
        self._path = path
        self._services: dict[str, ServiceDefinition] = {}
        self._user_domain = user_domain   # e.g. "alice.c.ozma.dev"
        self._client: httpx.AsyncClient | None = None
        self._health_task: asyncio.Task | None = None
        self._load()

    # ── Persistence ──────────────────────────────────────────────────

    def _load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text())
            for sd in data.get("services", []):
                s = ServiceDefinition.from_dict(sd)
                self._services[s.id] = s
            if data.get("user_domain"):
                self._user_domain = data["user_domain"]
            log.info("Loaded %d service(s) from %s", len(self._services), self._path.name)
        except Exception as e:
            log.warning("Failed to load services: %s", e)

    def _save(self) -> None:
        data: dict[str, Any] = {
            "services": [s.to_storage() for s in self._services.values()],
            "user_domain": self._user_domain,
        }
        tmp = self._path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2))
        tmp.replace(self._path)

    # ── Lifecycle ────────────────────────────────────────────────────

    async def start(self) -> None:
        self._client = httpx.AsyncClient(timeout=30.0, follow_redirects=True)
        self._health_task = asyncio.create_task(self._health_monitor(), name="service-health")

    async def stop(self) -> None:
        if self._health_task:
            self._health_task.cancel()
        if self._client:
            await self._client.aclose()

    # ── Service CRUD ─────────────────────────────────────────────────

    def register_service(self, name: str, owner_user_id: str,
                         target_host: str, target_port: int,
                         subdomain: str = "", protocol: str = "http",
                         service_type: str = "",
                         auth_required: bool = True,
                         health_path: str = "/health",
                         icon: str = "") -> ServiceDefinition:
        # Security: validate target host against SSRF blocklist
        validate_target_host(target_host)
        if protocol not in ("http", "https"):
            raise ValueError(f"Unsupported protocol: {protocol}")
        # Validate and normalise subdomain
        effective_subdomain = subdomain or name.lower().replace(" ", "-")
        validate_subdomain(effective_subdomain)
        if any(s.subdomain == effective_subdomain for s in self._services.values()):
            raise ValueError(f"Subdomain already taken: {effective_subdomain}")
        service = ServiceDefinition(
            id=str(uuid.uuid4()),
            name=name,
            owner_user_id=owner_user_id,
            target_host=target_host,
            target_port=target_port,
            protocol=protocol,
            subdomain=effective_subdomain,
            service_type=service_type,
            auth_required=auth_required,
            health_path=health_path,
            icon=icon,
            created_at=time.time(),
        )
        self._services[service.id] = service
        self._save()
        log.info("Registered service %s (%s) → %s:%d",
                 service.name, service.subdomain, target_host, target_port)
        return service

    def remove_service(self, service_id: str) -> bool:
        service = self._services.pop(service_id, None)
        if service:
            self._save()
            log.info("Removed service %s (%s)", service.name, service.id)
            return True
        return False

    def get_service(self, service_id: str) -> ServiceDefinition | None:
        return self._services.get(service_id)

    def list_services(self) -> list[ServiceDefinition]:
        return list(self._services.values())

    def update_service(self, service_id: str, **kwargs: Any) -> ServiceDefinition | None:
        service = self._services.get(service_id)
        if not service:
            return None
        # Security: re-validate target host and subdomain on update
        if "target_host" in kwargs:
            validate_target_host(kwargs["target_host"])
        if "protocol" in kwargs and kwargs["protocol"] not in ("http", "https"):
            raise ValueError(f"Unsupported protocol: {kwargs['protocol']}")
        if "subdomain" in kwargs:
            validate_subdomain(kwargs["subdomain"])
        for key in ("name", "target_host", "target_port", "protocol", "subdomain",
                     "auth_required", "health_path", "icon", "enabled"):
            if key in kwargs:
                setattr(service, key, kwargs[key])
        self._save()
        return service

    # ── Host-header matching ─────────────────────────────────────────

    def match_service(self, host: str) -> ServiceDefinition | None:
        """Match a Host header to a registered service.

        Matches against:
          - ``{subdomain}.{user_domain}``  (e.g. jellyfin.alice.c.ozma.dev)
          - ``{subdomain}.localhost``       (local dev)
        """
        host = host.split(":")[0].lower()  # strip port

        for service in self._services.values():
            if not service.enabled or not service.subdomain:
                continue
            # Match against user's Connect domain
            if self._user_domain and host == f"{service.subdomain}.{self._user_domain}":
                return service
            # Match against localhost for dev
            if host == f"{service.subdomain}.localhost":
                return service
        return None

    # ── Reverse proxy ────────────────────────────────────────────────

    async def proxy_request(self, request: Request, service: ServiceDefinition) -> Response:
        """Forward an HTTP request to the target service and stream the response back."""
        if not self._client:
            return Response(status_code=503, content="Proxy not started")

        target_url = f"{service.target_url()}{request.url.path}"
        if request.url.query:
            target_url += f"?{request.url.query}"

        # Security: build a clean header set — strip sensitive headers
        _STRIP_HEADERS = frozenset({
            "authorization", "cookie", "set-cookie",
            "x-forwarded-for", "x-forwarded-proto", "x-forwarded-host",
            "x-real-ip", "cf-connecting-ip", "true-client-ip",
            "content-length", "transfer-encoding", "host",
        })
        headers = {
            k: v for k, v in request.headers.items()
            if k.lower() not in _STRIP_HEADERS
        }
        headers["host"] = f"{service.target_host}:{service.target_port}"

        # Set X-Forwarded-* from trusted request properties only
        client_ip = request.client.host if request.client else "127.0.0.1"
        headers["x-forwarded-for"] = client_ip
        headers["x-forwarded-proto"] = request.url.scheme
        headers["x-forwarded-host"] = request.headers.get("host", "")

        body = await request.body()

        try:
            resp = await self._client.request(
                method=request.method,
                url=target_url,
                headers=headers,
                content=body,
            )
            # Stream the response back
            excluded_headers = {"content-encoding", "content-length", "transfer-encoding"}
            response_headers = {
                k: v for k, v in resp.headers.items()
                if k.lower() not in excluded_headers
            }
            return Response(
                content=resp.content,
                status_code=resp.status_code,
                headers=response_headers,
            )
        except httpx.ConnectError:
            return Response(status_code=502, content=f"Service {service.name} unreachable")
        except httpx.TimeoutException:
            return Response(status_code=504, content=f"Service {service.name} timed out")

    # ── Health monitoring ────────────────────────────────────────────

    async def _health_monitor(self) -> None:
        """Check health of all registered services every 30 seconds."""
        while True:
            await asyncio.sleep(30)
            if not self._client:
                continue
            for service in list(self._services.values()):
                if not service.enabled:
                    continue
                try:
                    url = f"{service.target_url()}{service.health_path}"
                    resp = await self._client.get(url, timeout=5.0)
                    was_healthy = service.healthy
                    service.healthy = 200 <= resp.status_code < 400
                    if was_healthy != service.healthy:
                        status = "healthy" if service.healthy else "unhealthy"
                        log.info("Service %s is now %s", service.name, status)
                except Exception:
                    if service.healthy:
                        log.info("Service %s is now unhealthy", service.name)
                    service.healthy = False
                service.last_health_check = time.time()

    async def check_health(self, service_id: str) -> dict[str, Any]:
        """On-demand health check for a single service."""
        service = self._services.get(service_id)
        if not service:
            return {"error": "Service not found"}
        if not self._client:
            return {"error": "Proxy not started"}
        try:
            url = f"{service.target_url()}{service.health_path}"
            resp = await self._client.get(url, timeout=5.0)
            service.healthy = 200 <= resp.status_code < 400
            service.last_health_check = time.time()
            return {
                "healthy": service.healthy,
                "status_code": resp.status_code,
                "checked_at": service.last_health_check,
            }
        except Exception as e:
            service.healthy = False
            service.last_health_check = time.time()
            return {"healthy": False, "error": str(e)}

    # ── Properties ───────────────────────────────────────────────────

    @property
    def user_domain(self) -> str:
        return self._user_domain

    @user_domain.setter
    def user_domain(self, value: str) -> None:
        self._user_domain = value
        self._save()
