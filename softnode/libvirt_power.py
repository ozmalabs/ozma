# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Libvirt power control backend for soft nodes.

Replaces QMP for VM power management when running under libvirt/Proxmox.
Uses the libvirt Python API for status queries and power actions, and
falls back to virsh subprocess calls if the library isn't available.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any, Protocol

log = logging.getLogger("ozma.softnode.libvirt_power")

try:
    import libvirt
    _LIBVIRT_AVAILABLE = True
except ImportError:
    _LIBVIRT_AVAILABLE = False


class PowerBackend(Protocol):
    """Interface for VM power control backends."""
    connected: bool
    async def query_status(self) -> dict[str, Any] | None: ...
    async def cont(self) -> bool: ...
    async def system_powerdown(self) -> bool: ...
    async def system_reset(self) -> bool: ...
    async def stop(self) -> bool: ...


class LibvirtPower:
    """Power control via libvirt API."""

    def __init__(self, domain_name: str, uri: str = "qemu:///system") -> None:
        self._domain_name = domain_name
        self._uri = uri
        self._conn: Any = None
        self._domain: Any = None
        self.connected = False

    async def start(self) -> bool:
        loop = asyncio.get_running_loop()
        try:
            self._conn, self._domain = await loop.run_in_executor(None, self._connect)
            self.connected = self._domain is not None
            if self.connected:
                log.info("libvirt power: connected to domain '%s'", self._domain_name)
            return self.connected
        except Exception as e:
            log.warning("libvirt power: failed to connect: %s", e)
            return False

    def _connect(self) -> tuple[Any, Any]:
        if not _LIBVIRT_AVAILABLE:
            return None, None
        conn = libvirt.open(self._uri)
        if not conn:
            return None, None
        try:
            domain = conn.lookupByName(self._domain_name)
            return conn, domain
        except libvirt.libvirtError:
            conn.close()
            return None, None

    async def query_status(self) -> dict[str, Any] | None:
        if not self._domain:
            return await self._virsh_status()
        loop = asyncio.get_running_loop()
        try:
            state, _ = await loop.run_in_executor(None, self._domain.state)
            status_map = {
                1: "running",    # VIR_DOMAIN_RUNNING
                2: "blocked",    # VIR_DOMAIN_BLOCKED
                3: "paused",     # VIR_DOMAIN_PAUSED
                4: "shutdown",   # VIR_DOMAIN_SHUTDOWN
                5: "shutoff",    # VIR_DOMAIN_SHUTOFF
                6: "crashed",    # VIR_DOMAIN_CRASHED
                7: "pmsuspended",
            }
            return {"status": status_map.get(state, "unknown")}
        except Exception as e:
            log.debug("libvirt status query failed: %s", e)
            return await self._virsh_status()

    async def cont(self) -> bool:
        """Resume a paused VM, or start a shut-off VM."""
        if self._domain:
            loop = asyncio.get_running_loop()
            try:
                state, _ = await loop.run_in_executor(None, self._domain.state)
                if state == 3:  # paused
                    await loop.run_in_executor(None, self._domain.resume)
                elif state == 5:  # shutoff
                    await loop.run_in_executor(None, self._domain.create)
                return True
            except Exception as e:
                log.warning("libvirt cont failed: %s", e)
        return await self._virsh("resume") or await self._virsh("start")

    async def system_powerdown(self) -> bool:
        if self._domain:
            loop = asyncio.get_running_loop()
            try:
                await loop.run_in_executor(None, self._domain.shutdown)
                return True
            except Exception as e:
                log.warning("libvirt shutdown failed: %s", e)
        return await self._virsh("shutdown")

    async def system_reset(self) -> bool:
        if self._domain:
            loop = asyncio.get_running_loop()
            try:
                await loop.run_in_executor(None, self._domain.reset)
                return True
            except Exception as e:
                log.warning("libvirt reset failed: %s", e)
        return await self._virsh("reset")

    async def stop(self) -> bool:
        """Force-stop (destroy) the VM."""
        if self._domain:
            loop = asyncio.get_running_loop()
            try:
                await loop.run_in_executor(None, self._domain.destroy)
                return True
            except Exception as e:
                log.warning("libvirt destroy failed: %s", e)
        return await self._virsh("destroy")

    async def close(self) -> None:
        if self._conn:
            try:
                self._conn.close()
            except Exception:
                pass
            self._conn = None
            self._domain = None
            self.connected = False

    # -- virsh subprocess fallback --

    async def _virsh(self, action: str) -> bool:
        try:
            proc = await asyncio.create_subprocess_exec(
                "virsh", action, self._domain_name,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
            )
            _, stderr = await proc.communicate()
            if proc.returncode == 0:
                return True
            log.debug("virsh %s %s failed: %s", action, self._domain_name, stderr.decode().strip())
            return False
        except FileNotFoundError:
            return False

    async def _virsh_status(self) -> dict[str, Any] | None:
        try:
            proc = await asyncio.create_subprocess_exec(
                "virsh", "domstate", self._domain_name,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            stdout, _ = await proc.communicate()
            if proc.returncode == 0:
                return {"status": stdout.decode().strip()}
        except FileNotFoundError:
            pass
        return None
