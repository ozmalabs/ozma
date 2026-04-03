# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Local reverse proxy — Caddy-based HTTPS for LAN services.

Runs a Caddy instance that terminates HTTPS for local services, using
Caddy's built-in CA for self-signed certificates (trusted on the LAN
with one-time CA import, or via mkcert if installed).

Typical use cases:
  jellyfin.home   → http://localhost:8096
  vaultwarden.home → http://localhost:8080
  frigate.home    → http://localhost:5000

Connect handles remote (internet) HTTPS access via subdomain proxy.
This module handles LAN-side HTTPS — no internet required, no cert fees.

Architecture
────────────
  LocalProxyManager manages a Caddy process.
  Routes are written to a Caddyfile in /tmp/ozma-caddy/.
  Caddy is reloaded (caddy reload --config ...) on route changes.
  TLS modes:
    internal  — Caddy's internal CA (self-signed, works immediately)
    acme      — Let's Encrypt (requires public DNS, port 80/443 open)
    off       — plain HTTP only

Usage
─────
  proxy = LocalProxyManager()
  await proxy.start()
  proxy.add_route("jellyfin", "jellyfin.home", "http://localhost:8096")
  await proxy.apply()
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.local_proxy")

CADDY_CONF_DIR  = Path("/tmp/ozma-caddy")
CADDY_CONF_FILE = CADDY_CONF_DIR / "Caddyfile"
CADDY_DATA_DIR  = Path("/var/lib/ozma/caddy-data")
CADDY_LOG_FILE  = Path("/tmp/ozma-caddy.log")


# ---------------------------------------------------------------------------
# Data models
# ---------------------------------------------------------------------------

@dataclass
class ProxyRoute:
    id:           str
    name:         str
    match_domain: str         # hostname Caddy listens on, e.g. "jellyfin.home"
    upstream:     str         # e.g. "http://localhost:8096"
    tls_mode:     str = "internal"   # "internal" | "acme" | "off"
    enabled:      bool = True
    strip_prefix: str = ""           # optional path prefix to strip
    extra_headers: dict[str, str] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id":           self.id,
            "name":         self.name,
            "match_domain": self.match_domain,
            "upstream":     self.upstream,
            "tls_mode":     self.tls_mode,
            "enabled":      self.enabled,
            "strip_prefix": self.strip_prefix,
            "extra_headers": self.extra_headers,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "ProxyRoute":
        return cls(
            id           = d["id"],
            name         = d.get("name", d["id"]),
            match_domain = d["match_domain"],
            upstream     = d["upstream"],
            tls_mode     = d.get("tls_mode", "internal"),
            enabled      = d.get("enabled", True),
            strip_prefix = d.get("strip_prefix", ""),
            extra_headers = d.get("extra_headers", {}),
        )


@dataclass
class LocalProxyConfig:
    enabled:       bool = False
    bind_address:  str  = "0.0.0.0"
    http_port:     int  = 80
    https_port:    int  = 443
    caddy_binary:  str  = "caddy"
    admin_api:     str  = "localhost:2019"    # Caddy admin API
    # Internal CA cert path (for dashboard download / client trust)
    ca_cert_path:  str  = str(CADDY_DATA_DIR / "pki/authorities/local/root.crt")

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled":      self.enabled,
            "bind_address": self.bind_address,
            "http_port":    self.http_port,
            "https_port":   self.https_port,
            "caddy_binary": self.caddy_binary,
            "admin_api":    self.admin_api,
            "ca_cert_path": self.ca_cert_path,
        }

    @classmethod
    def from_dict(cls, d: dict) -> "LocalProxyConfig":
        return cls(
            enabled      = d.get("enabled", False),
            bind_address = d.get("bind_address", "0.0.0.0"),
            http_port    = d.get("http_port", 80),
            https_port   = d.get("https_port", 443),
            caddy_binary = d.get("caddy_binary", "caddy"),
            admin_api    = d.get("admin_api", "localhost:2019"),
            ca_cert_path = d.get("ca_cert_path", str(CADDY_DATA_DIR / "pki/authorities/local/root.crt")),
        )


# ---------------------------------------------------------------------------
# Caddyfile generation
# ---------------------------------------------------------------------------

def build_caddyfile(
    routes: list[ProxyRoute],
    config: LocalProxyConfig,
) -> str:
    """
    Generate a Caddyfile for the given routes.

    Each route becomes a Caddy site block:
        jellyfin.home {
            tls internal
            reverse_proxy http://localhost:8096
        }
    """
    enabled = [r for r in routes if r.enabled]
    if not enabled:
        # Minimal Caddyfile — Caddy won't start without at least a global block
        return f"""{{
    admin {config.admin_api}
    storage file_system {{
        root {CADDY_DATA_DIR}
    }}
}}
"""

    blocks: list[str] = [
        f"""{{
    admin {config.admin_api}
    storage file_system {{
        root {CADDY_DATA_DIR}
    }}
}}
""",
    ]

    for route in enabled:
        tls_line = ""
        if route.tls_mode == "internal":
            tls_line = "    tls internal"
        elif route.tls_mode == "off":
            tls_line = ""
        else:
            # acme — just let Caddy handle it with HTTPS default
            tls_line = ""

        headers = ""
        if route.extra_headers:
            header_lines = "\n".join(
                f'        {k} {v}' for k, v in route.extra_headers.items()
            )
            headers = f"""
    header {{
{header_lines}
    }}"""

        strip = ""
        if route.strip_prefix:
            strip = f"\n    uri strip_prefix {route.strip_prefix}"

        scheme = "https" if route.tls_mode != "off" else "http"
        bind   = config.bind_address
        port   = config.https_port if route.tls_mode != "off" else config.http_port

        addr = (
            f"http://{route.match_domain}:{port}"
            if route.tls_mode == "off"
            else f"https://{route.match_domain}:{port}"
        )

        block = f"""{addr} {{
    bind {bind}
{tls_line}
    reverse_proxy {route.upstream} {{{strip}
        header_up Host {{upstream_hostport}}
        header_up X-Forwarded-For {{remote_host}}
        header_up X-Forwarded-Proto {scheme}
    }}{headers}
}}
"""
        blocks.append(block)

    return "\n".join(blocks)


# ---------------------------------------------------------------------------
# Manager
# ---------------------------------------------------------------------------

class LocalProxyManager:
    """Manages a local Caddy reverse proxy for LAN HTTPS."""

    STATE_PATH = Path("/var/lib/ozma/local_proxy_state.json")

    def __init__(self, state_path: Path | None = None) -> None:
        self._state_path = state_path or self.STATE_PATH
        self._config     = LocalProxyConfig()
        self._routes:    dict[str, ProxyRoute] = {}
        self._proc:      asyncio.subprocess.Process | None = None
        self._active     = False
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._load()
        if self._config.enabled:
            await self._apply()
        log.info("LocalProxyManager started (enabled=%s, routes=%d)",
                 self._config.enabled, len(self._routes))

    async def stop(self) -> None:
        await self._teardown()

    async def _apply(self) -> None:
        CADDY_CONF_DIR.mkdir(parents=True, exist_ok=True)
        CADDY_DATA_DIR.mkdir(parents=True, exist_ok=True)
        caddyfile = build_caddyfile(list(self._routes.values()), self._config)
        CADDY_CONF_FILE.write_text(caddyfile)
        CADDY_CONF_FILE.chmod(0o600)

        if self._proc and self._proc.returncode is None:
            await self._reload()
        else:
            await self._start_caddy()
        self._active = True

    async def _teardown(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=5.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        self._proc   = None
        self._active = False

    # ------------------------------------------------------------------
    # Caddy process management
    # ------------------------------------------------------------------

    async def _start_caddy(self) -> None:
        caddy = self._config.caddy_binary
        try:
            self._proc = await asyncio.create_subprocess_exec(
                caddy, "run",
                "--config", str(CADDY_CONF_FILE),
                "--adapter", "caddyfile",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            log.info("Caddy started (pid=%d)", self._proc.pid)
            asyncio.create_task(self._drain_stderr(), name="caddy.stderr")
        except FileNotFoundError:
            log.warning("caddy not installed — local proxy unavailable (apt install caddy)")
        except Exception as exc:
            log.error("Caddy failed to start: %s", exc)

    async def _reload(self) -> None:
        """Reload Caddy config without restarting (zero-downtime)."""
        caddy = self._config.caddy_binary
        try:
            proc = await asyncio.create_subprocess_exec(
                caddy, "reload",
                "--config", str(CADDY_CONF_FILE),
                "--adapter", "caddyfile",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, err = await asyncio.wait_for(proc.communicate(), timeout=10.0)
            if proc.returncode != 0:
                log.error("caddy reload failed: %s", err.decode(errors="replace").strip())
            else:
                log.info("Caddy config reloaded (%d active routes)",
                         sum(1 for r in self._routes.values() if r.enabled))
        except asyncio.TimeoutError:
            log.error("caddy reload timed out")
        except Exception as exc:
            log.error("caddy reload: %s", exc)

    async def _drain_stderr(self) -> None:
        if not self._proc or not self._proc.stderr:
            return
        try:
            async for line in self._proc.stderr:
                log.debug("caddy: %s", line.decode(errors="replace").rstrip())
        except Exception:
            pass

    # ------------------------------------------------------------------
    # Route management
    # ------------------------------------------------------------------

    def add_route(
        self,
        name: str,
        match_domain: str,
        upstream: str,
        tls_mode: str = "internal",
        strip_prefix: str = "",
        extra_headers: dict | None = None,
    ) -> ProxyRoute:
        route_id = re.sub(r"[^a-z0-9\-]", "-", name.lower())[:40]
        if route_id in self._routes:
            route_id = f"{route_id}-{int(time.time())}"
        route = ProxyRoute(
            id           = route_id,
            name         = name,
            match_domain = match_domain.lower(),
            upstream     = upstream,
            tls_mode     = tls_mode,
            strip_prefix = strip_prefix,
            extra_headers = extra_headers or {},
        )
        self._routes[route_id] = route
        self._save()
        return route

    def update_route(self, route_id: str, **kwargs: Any) -> ProxyRoute | None:
        route = self._routes.get(route_id)
        if not route:
            return None
        for k, v in kwargs.items():
            if hasattr(route, k):
                setattr(route, k, v)
        self._save()
        return route

    def remove_route(self, route_id: str) -> bool:
        if route_id not in self._routes:
            return False
        del self._routes[route_id]
        self._save()
        return True

    def list_routes(self) -> list[dict]:
        return [r.to_dict() for r in self._routes.values()]

    def get_route(self, route_id: str) -> ProxyRoute | None:
        return self._routes.get(route_id)

    async def apply(self) -> None:
        """Write Caddyfile and reload/start Caddy."""
        if self._config.enabled:
            await self._apply()

    # ------------------------------------------------------------------
    # Configuration
    # ------------------------------------------------------------------

    def get_config(self) -> LocalProxyConfig:
        return self._config

    async def set_config(self, **kwargs: Any) -> LocalProxyConfig:
        was_enabled = self._config.enabled
        for k, v in kwargs.items():
            if hasattr(self._config, k):
                setattr(self._config, k, v)
        self._save()
        if self._config.enabled and not was_enabled:
            await self._apply()
        elif was_enabled and not self._config.enabled:
            await self._teardown()
        elif self._config.enabled and self._active:
            await self._apply()
        return self._config

    def get_status(self) -> dict[str, Any]:
        enabled_routes = sum(1 for r in self._routes.values() if r.enabled)
        return {
            "enabled":        self._config.enabled,
            "active":         self._active,
            "caddy_running":  bool(self._proc and self._proc.returncode is None),
            "routes_total":   len(self._routes),
            "routes_enabled": enabled_routes,
            "admin_api":      self._config.admin_api,
            "ca_cert_path":   self._config.ca_cert_path,
        }

    def get_ca_cert(self) -> bytes | None:
        """Return the Caddy internal CA certificate (for client trust installation)."""
        p = Path(self._config.ca_cert_path)
        if p.exists():
            return p.read_bytes()
        return None

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self._state_path.with_suffix(".tmp")
        data = {
            "config": self._config.to_dict(),
            "routes": {rid: r.to_dict() for rid, r in self._routes.items()},
        }
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(self._state_path)

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text())
            self._config = LocalProxyConfig.from_dict(data.get("config", {}))
            for rid, rd in data.get("routes", {}).items():
                self._routes[rid] = ProxyRoute.from_dict(rd)
        except Exception:
            log.exception("Failed to load local proxy state")
