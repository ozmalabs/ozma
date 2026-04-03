# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Vaultwarden manager — lifecycle management for a Vaultwarden password manager instance.

Vaultwarden (https://github.com/dani-garcia/vaultwarden) is a Bitwarden-compatible
self-hosted password manager written in Rust.  The controller manages it as a Docker
container, routes external access via the service proxy (vault.{user}.c.ozma.dev),
and integrates with the OIDC IdP so users sign in with their Ozma account.

The SQLite database + attachments + RSA key material are included in the backup
schedule automatically — see backup_paths() for what the backup module needs to archive.

Configuration:
  OZMA_VAULTWARDEN=1            Enable Vaultwarden (default: off)
  OZMA_VAULTWARDEN_DATA         Path to persistent data dir (default: ./vaultwarden-data)
  OZMA_VAULTWARDEN_PORT         Internal HTTP port (default: 8222)
  OZMA_VAULTWARDEN_ADMIN_TOKEN  Admin panel token (auto-generated if empty)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import secrets
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.vaultwarden")

_CONTAINER_NAME = "ozma-vaultwarden"
_DEFAULT_PORT = 8222
_HEALTH_INTERVAL = 30.0   # seconds between health checks
_START_TIMEOUT = 30.0     # seconds to wait for container to become healthy


@dataclass
class VaultwardenConfig:
    enabled: bool = False
    data_dir: str = ""              # absolute path; empty → <controller_dir>/vaultwarden-data
    port: int = _DEFAULT_PORT       # internal HTTP port (proxied externally)
    admin_token: str = ""           # hashed admin token; auto-generated if empty
    oidc_enabled: bool = False      # enable SSO via controller IdP
    oidc_client_id: str = ""        # OIDC client_id registered with the IdP
    oidc_client_secret: str = ""    # OIDC client_secret
    oidc_issuer_url: str = ""       # e.g. https://alice.c.ozma.dev/auth
    signup_disabled: bool = True    # disable open registration (use OIDC or invitations)
    org_creation_disabled: bool = False

    @classmethod
    def from_env(cls) -> "VaultwardenConfig":
        return cls(
            enabled=os.environ.get("OZMA_VAULTWARDEN", "0").lower() in ("1", "true", "yes"),
            data_dir=os.environ.get("OZMA_VAULTWARDEN_DATA", ""),
            port=int(os.environ.get("OZMA_VAULTWARDEN_PORT", str(_DEFAULT_PORT))),
            admin_token=os.environ.get("OZMA_VAULTWARDEN_ADMIN_TOKEN", ""),
            oidc_enabled=os.environ.get("OZMA_VAULTWARDEN_OIDC", "0").lower() in ("1", "true", "yes"),
            oidc_client_id=os.environ.get("OZMA_VAULTWARDEN_OIDC_CLIENT_ID", ""),
            oidc_client_secret=os.environ.get("OZMA_VAULTWARDEN_OIDC_SECRET", ""),
            oidc_issuer_url=os.environ.get("OZMA_VAULTWARDEN_OIDC_ISSUER", ""),
        )


@dataclass
class VaultwardenStatus:
    running: bool = False
    container_id: str = ""
    port: int = 0
    version: str = ""
    admin_panel_url: str = ""
    vault_url: str = ""
    oidc_enabled: bool = False
    last_healthy: float = 0.0
    error: str = ""


class VaultwardenManager:
    """Manages a Vaultwarden Docker container alongside the controller.

    Lifecycle:
        mgr = VaultwardenManager(config)
        await mgr.start()
        # ... controller runs ...
        await mgr.stop()
    """

    def __init__(self, config: VaultwardenConfig | None = None,
                 controller_dir: Path | None = None) -> None:
        self._cfg = config or VaultwardenConfig()
        self._controller_dir = controller_dir or Path(__file__).parent
        self._status = VaultwardenStatus()
        self._health_task: asyncio.Task | None = None
        self._token_file = self._controller_dir / "vaultwarden_admin_token.txt"

    # ── Public API ───────────────────────────────────────────────────────────

    async def start(self) -> None:
        if not self._cfg.enabled:
            log.debug("Vaultwarden disabled — skipping")
            return

        if not await self._docker_available():
            log.warning("Docker not found — Vaultwarden requires Docker; skipping")
            return

        data_dir = Path(self._cfg.data_dir) if self._cfg.data_dir else (
            self._controller_dir / "vaultwarden-data"
        )
        data_dir.mkdir(parents=True, exist_ok=True)

        admin_token = await self._ensure_admin_token()

        try:
            await self._ensure_container(data_dir, admin_token)
        except Exception as e:
            log.error("Failed to start Vaultwarden container: %s", e)
            self._status.error = str(e)
            return

        # Wait for the container to become healthy
        if not await self._wait_healthy():
            log.error("Vaultwarden container did not become healthy within %ss", _START_TIMEOUT)
            self._status.error = "container did not become healthy"
            return

        self._status.port = self._cfg.port
        self._status.admin_panel_url = f"http://localhost:{self._cfg.port}/admin"
        self._status.vault_url = f"http://localhost:{self._cfg.port}"
        self._status.oidc_enabled = self._cfg.oidc_enabled

        self._health_task = asyncio.create_task(self._health_loop(), name="vaultwarden-health")
        log.info("Vaultwarden running on port %d", self._cfg.port)

    async def stop(self) -> None:
        if self._health_task:
            self._health_task.cancel()
            try:
                await self._health_task
            except asyncio.CancelledError:
                pass
        # Container is left running — it persists across controller restarts.
        # The user must explicitly remove it via Docker if they want to stop it.

    def get_status(self) -> dict[str, Any]:
        return {
            "running": self._status.running,
            "container_id": self._status.container_id,
            "port": self._status.port,
            "version": self._status.version,
            "admin_panel_url": self._status.admin_panel_url,
            "vault_url": self._status.vault_url,
            "oidc_enabled": self._status.oidc_enabled,
            "last_healthy": self._status.last_healthy,
            "error": self._status.error,
        }

    def backup_paths(self) -> list[str]:
        """Return paths that must be included in the Ozma backup schedule."""
        data_dir = Path(self._cfg.data_dir) if self._cfg.data_dir else (
            self._controller_dir / "vaultwarden-data"
        )
        return [
            str(data_dir / "db.sqlite3"),
            str(data_dir / "db.sqlite3-wal"),
            str(data_dir / "db.sqlite3-shm"),
            str(data_dir / "attachments"),
            str(data_dir / "sends"),
            str(data_dir / "rsa_key.pem"),
            str(data_dir / "rsa_key.pub.pem"),
        ]

    def configure_oidc(self, client_id: str, client_secret: str, issuer_url: str) -> None:
        """Update OIDC config at runtime.  Takes effect on next container restart."""
        self._cfg.oidc_enabled = True
        self._cfg.oidc_client_id = client_id
        self._cfg.oidc_client_secret = client_secret
        self._cfg.oidc_issuer_url = issuer_url
        self._status.oidc_enabled = True

    # ── Internal helpers ─────────────────────────────────────────────────────

    async def _docker_available(self) -> bool:
        try:
            proc = await asyncio.create_subprocess_exec(
                "docker", "info",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()
            return proc.returncode == 0
        except FileNotFoundError:
            return False

    async def _ensure_admin_token(self) -> str:
        """Return the admin token, generating and persisting one if absent."""
        if self._cfg.admin_token:
            return self._cfg.admin_token

        if self._token_file.exists():
            token = self._token_file.read_text().strip()
            if token:
                return token

        token = secrets.token_urlsafe(32)
        # Set permissions before writing — file is sensitive
        self._token_file.touch(mode=0o600)
        self._token_file.write_text(token)
        log.info("Generated Vaultwarden admin token — stored in %s", self._token_file)
        return token

    async def _ensure_container(self, data_dir: Path, admin_token: str) -> None:
        """Start the Vaultwarden container if it's not already running."""
        # Check if container already running
        running = await self._container_running()
        if running:
            log.debug("Vaultwarden container already running")
            self._status.running = True
            self._status.container_id = running
            return

        # Remove any stopped container with the same name
        await self._run_docker("rm", "-f", _CONTAINER_NAME)

        env_vars: list[str] = [
            f"ROCKET_PORT={self._cfg.port}",
            f"ADMIN_TOKEN={admin_token}",
            "WEBSOCKET_ENABLED=true",
            "SIGNUPS_ALLOWED=" + ("false" if self._cfg.signup_disabled else "true"),
            "ORG_CREATION_USERS=" + ("" if not self._cfg.org_creation_disabled else "none"),
        ]

        if self._cfg.oidc_enabled and self._cfg.oidc_client_id:
            env_vars += [
                f"SSO_ENABLED=true",
                f"SSO_CLIENT_ID={self._cfg.oidc_client_id}",
                f"SSO_CLIENT_SECRET={self._cfg.oidc_client_secret}",
                f"SSO_AUTHORITY={self._cfg.oidc_issuer_url}",
                "SSO_ONLY=false",  # allow password fallback for API clients
            ]

        cmd = [
            "docker", "run", "-d",
            "--name", _CONTAINER_NAME,
            "--restart", "unless-stopped",
            "-v", f"{data_dir}:/data",
            "-p", f"127.0.0.1:{self._cfg.port}:{self._cfg.port}",
        ]
        for ev in env_vars:
            cmd += ["-e", ev]
        cmd.append("vaultwarden/server:latest")

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await proc.communicate()
        if proc.returncode != 0:
            raise RuntimeError(f"docker run failed: {stderr.decode().strip()}")

        container_id = stdout.decode().strip()
        self._status.container_id = container_id
        log.info("Started Vaultwarden container: %s", container_id[:12])

    async def _container_running(self) -> str:
        """Return container ID if running, empty string otherwise."""
        proc = await asyncio.create_subprocess_exec(
            "docker", "inspect",
            "--format", "{{.State.Running}}|{{.Id}}",
            _CONTAINER_NAME,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
        stdout, _ = await proc.communicate()
        if proc.returncode != 0:
            return ""
        parts = stdout.decode().strip().split("|")
        if len(parts) == 2 and parts[0] == "true":
            return parts[1][:12]
        return ""

    async def _run_docker(self, *args: str) -> None:
        """Run a docker command, ignoring errors (used for cleanup)."""
        proc = await asyncio.create_subprocess_exec(
            "docker", *args,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        await proc.wait()

    async def _wait_healthy(self) -> bool:
        """Poll the Vaultwarden health endpoint until it responds or times out."""
        deadline = asyncio.get_event_loop().time() + _START_TIMEOUT
        url = f"http://localhost:{self._cfg.port}/alive"
        while asyncio.get_event_loop().time() < deadline:
            try:
                import urllib.request
                loop = asyncio.get_event_loop()
                def _check():
                    try:
                        with urllib.request.urlopen(url, timeout=2) as r:
                            return r.status < 500
                    except Exception:
                        return False
                if await loop.run_in_executor(None, _check):
                    self._status.running = True
                    self._status.last_healthy = time.monotonic()
                    return True
            except Exception:
                pass
            await asyncio.sleep(1.0)
        return False

    async def _health_loop(self) -> None:
        """Background task: check Vaultwarden health every 30 seconds."""
        import urllib.request
        url = f"http://localhost:{self._cfg.port}/alive"
        loop = asyncio.get_event_loop()
        while True:
            await asyncio.sleep(_HEALTH_INTERVAL)
            try:
                def _check():
                    try:
                        with urllib.request.urlopen(url, timeout=5) as r:
                            return r.status < 500
                    except Exception:
                        return False
                ok = await loop.run_in_executor(None, _check)
                if ok:
                    self._status.running = True
                    self._status.last_healthy = time.monotonic()
                    self._status.error = ""
                else:
                    self._status.running = False
                    self._status.error = "health check failed"
                    log.warning("Vaultwarden health check failed — container may have crashed")
            except asyncio.CancelledError:
                raise
            except Exception as e:
                log.debug("Vaultwarden health check error: %s", e)
