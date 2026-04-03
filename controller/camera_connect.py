# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
V1.7 Camera Connect registration — controller-side proxy for camera nodes.

Camera nodes (machine_class="camera") are often headless devices — a
Hikvision camera, a Frigate NVR, an auto-configured PoE camera. They have
no ozma agent and cannot self-register with Connect.

This module watches the controller's node state for camera nodes and acts
as their proxy to the Ozma Connect cloud service:

  1. Registration — POST /cameras/register for each camera node that comes
     online; records a Connect-assigned camera_id + relay config.

  2. Heartbeat — sends last_seen, online status, recording state, stream
     count every 60s for all registered cameras.

  3. WireGuard relay — requests a relay tunnel config so the camera's RTSP
     streams are reachable from anywhere with internet access.

  4. Deregistration — marks cameras offline in Connect when they leave the
     local node list (power-off, network loss).

Camera nodes appear in the Connect cloud dashboard as a fleet alongside
controllers and agents. A user with the app can tap a camera in Berlin
while the camera is in their holiday home in another city — the relay
forwards the RTSP stream through Connect's edge.

No local feature gating. Connect is optional. Without Connect the cameras
work identically on the local network.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from connect import OzmaConnect
    from state import AppState, NodeInfo

log = logging.getLogger("ozma.camera_connect")

_HEARTBEAT_INTERVAL = 60.0     # seconds between fleet heartbeats
_REGISTER_RETRY     = 30.0     # retry interval on registration failure
_OFFLINE_GRACE      = 90.0     # seconds after last_seen before marking offline


# ---------------------------------------------------------------------------
# Per-camera registration record
# ---------------------------------------------------------------------------

@dataclass
class CameraRegistration:
    """Tracks the Connect-side registration state of one camera node."""
    node_id: str
    camera_id: str = ""          # assigned by Connect on register
    relay_endpoint: str = ""     # WireGuard endpoint for remote RTSP
    relay_pubkey: str = ""       # WireGuard public key (Connect side)
    relay_allowed_ips: str = ""  # allowed IPs for the tunnel
    registered_at: float = 0.0
    last_heartbeat: float = 0.0
    online: bool = True
    error: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id":           self.node_id,
            "camera_id":         self.camera_id,
            "relay_endpoint":    self.relay_endpoint,
            "relay_pubkey":      self.relay_pubkey,
            "relay_allowed_ips": self.relay_allowed_ips,
            "registered_at":     self.registered_at,
            "last_heartbeat":    self.last_heartbeat,
            "online":            self.online,
            "error":             self.error,
        }

    @classmethod
    def from_dict(cls, d: dict[str, Any]) -> "CameraRegistration":
        obj = cls(node_id=d["node_id"])
        for k, v in d.items():
            if hasattr(obj, k):
                setattr(obj, k, v)
        return obj


# ---------------------------------------------------------------------------
# CameraConnectManager
# ---------------------------------------------------------------------------

class CameraConnectManager:
    """
    Controller-side proxy for camera node registration with Ozma Connect.

    Lifecycle:
      await mgr.start()
      ...
      await mgr.stop()

    The manager watches state.nodes for camera nodes (machine_class="camera")
    and registers/deregisters them with Connect automatically.

    When Connect is not configured or not authenticated, all operations
    are silently skipped — the cameras still work locally.
    """

    def __init__(
        self,
        state: "AppState",
        connect: "OzmaConnect | None",
        data_dir: Path | None = None,
    ) -> None:
        self._state = state
        self._connect = connect
        self._data_dir = data_dir or Path("/var/lib/ozma/camera_connect")
        self._registrations: dict[str, CameraRegistration] = {}  # node_id → reg
        self._tasks: list[asyncio.Task] = []
        self._load()

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        self._tasks.append(
            asyncio.create_task(self._registration_loop(), name="camera-connect:register")
        )
        self._tasks.append(
            asyncio.create_task(self._heartbeat_loop(), name="camera-connect:heartbeat")
        )
        log.info("CameraConnectManager started")

    async def stop(self) -> None:
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        self._tasks.clear()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def list_registrations(self) -> list[dict[str, Any]]:
        return [r.to_dict() for r in self._registrations.values()]

    def get_registration(self, node_id: str) -> CameraRegistration | None:
        return self._registrations.get(node_id)

    async def force_register(self, node_id: str) -> dict[str, Any]:
        """Immediately attempt (re-)registration of a camera node with Connect."""
        node = self._state.nodes.get(node_id)
        if not node or node.machine_class != "camera":
            return {"ok": False, "error": "Node not found or not a camera"}
        result = await self._register_camera(node)
        self._save()
        return result

    async def deregister(self, node_id: str) -> dict[str, Any]:
        """Remove a camera from Connect and clear its local registration record."""
        reg = self._registrations.get(node_id)
        if not reg:
            return {"ok": False, "error": "No registration for this node"}
        if reg.camera_id and self._connect and self._connect.authenticated:
            await self._connect_delete(f"/cameras/{reg.camera_id}")
        del self._registrations[node_id]
        self._save()
        log.info("Deregistered camera %s from Connect", node_id)
        return {"ok": True, "node_id": node_id}

    def relay_rtsp_url(self, node_id: str, stream_path: str = "/") -> str | None:
        """Return the Connect relay RTSP URL for a camera node, or None if no relay."""
        reg = self._registrations.get(node_id)
        if not reg or not reg.relay_endpoint:
            return None
        host = reg.relay_endpoint.split(":")[0]
        # RTSP via relay tunnel — the relay forwards to the camera's local IP
        return f"rtsp://{host}{stream_path}"

    # ------------------------------------------------------------------
    # Internal: registration loop
    # ------------------------------------------------------------------

    async def _registration_loop(self) -> None:
        """Periodically check for unregistered camera nodes and register them."""
        await asyncio.sleep(5.0)  # give discovery time to populate state
        while True:
            try:
                await self._check_and_register_all()
            except asyncio.CancelledError:
                break
            except Exception:
                log.exception("Camera registration loop error")
            await asyncio.sleep(_REGISTER_RETRY)

    async def _check_and_register_all(self) -> None:
        """Register any camera node that doesn't have a Connect record yet."""
        if not self._connect or not self._connect.authenticated:
            return

        for node_id, node in list(self._state.nodes.items()):
            if node.machine_class != "camera":
                continue
            reg = self._registrations.get(node_id)
            if reg and reg.camera_id and not reg.error:
                continue  # already registered
            await self._register_camera(node)

        self._save()

    async def _register_camera(self, node: "NodeInfo") -> dict[str, Any]:
        """Register one camera node with Connect. Returns result dict."""
        node_id = node.id
        payload = {
            "node_id":       node_id,
            "host":          node.host,
            "vendor":        node.hw,
            "camera_streams": node.camera_streams,
            "capabilities":  node.capabilities,
            "frigate_host":  node.frigate_host or "",
            "frigate_port":  node.frigate_port or 0,
        }
        result = await self._connect_post("/cameras/register", payload)
        if not result:
            err = "Connect API unavailable"
            self._registrations[node_id] = CameraRegistration(
                node_id=node_id, error=err
            )
            return {"ok": False, "error": err}

        camera_id = result.get("camera_id", "")
        relay = result.get("relay", {})

        reg = CameraRegistration(
            node_id=node_id,
            camera_id=camera_id,
            relay_endpoint=relay.get("endpoint", ""),
            relay_pubkey=relay.get("pubkey", ""),
            relay_allowed_ips=relay.get("allowed_ips", ""),
            registered_at=time.time(),
            online=True,
        )
        self._registrations[node_id] = reg

        log.info("Camera %s registered with Connect (camera_id=%s relay=%s)",
                 node_id, camera_id, reg.relay_endpoint or "none")
        return {"ok": True, "camera_id": camera_id, "relay_endpoint": reg.relay_endpoint}

    # ------------------------------------------------------------------
    # Internal: heartbeat loop
    # ------------------------------------------------------------------

    async def _heartbeat_loop(self) -> None:
        """Send periodic heartbeats for all registered cameras."""
        await asyncio.sleep(15.0)
        while True:
            try:
                await self._send_heartbeats()
            except asyncio.CancelledError:
                break
            except Exception:
                log.exception("Camera heartbeat loop error")
            await asyncio.sleep(_HEARTBEAT_INTERVAL)

    async def _send_heartbeats(self) -> None:
        if not self._connect or not self._connect.authenticated:
            return
        if not self._registrations:
            return

        now_mono = time.monotonic()
        now_wall = time.time()
        online_ids: list[str] = []
        offline_ids: list[str] = []

        for node_id, reg in list(self._registrations.items()):
            if not reg.camera_id:
                continue
            node = self._state.nodes.get(node_id)
            is_online = node is not None and (now_mono - node.last_seen) < _OFFLINE_GRACE
            if is_online:
                online_ids.append(reg.camera_id)
            else:
                offline_ids.append(reg.camera_id)
            reg.online = is_online

        if not online_ids and not offline_ids:
            return

        payload = {
            "online":  online_ids,
            "offline": offline_ids,
            "ts":      now_wall,
        }
        result = await self._connect_post("/cameras/heartbeat", payload)
        if result:
            for node_id, reg in self._registrations.items():
                if reg.camera_id in online_ids or reg.camera_id in offline_ids:
                    reg.last_heartbeat = now_wall
            log.debug("Camera heartbeat: %d online, %d offline",
                      len(online_ids), len(offline_ids))

    # ------------------------------------------------------------------
    # Internal: Connect HTTP helpers
    # ------------------------------------------------------------------

    async def _connect_post(self, path: str, body: dict) -> dict | None:
        if not self._connect:
            return None
        try:
            return await self._connect._api_post(path, body)
        except Exception as e:
            log.debug("Connect POST %s failed: %s", path, e)
            return None

    async def _connect_delete(self, path: str) -> None:
        if not self._connect:
            return
        try:
            loop = asyncio.get_running_loop()
            import urllib.request
            req = urllib.request.Request(
                f"{self._connect._api_base}{path}",
                method="DELETE",
                headers={"Authorization": f"Bearer {self._connect._token}"},
            )
            await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=10))
        except Exception as e:
            log.debug("Connect DELETE %s failed: %s", path, e)

    # ------------------------------------------------------------------
    # Persistence
    # ------------------------------------------------------------------

    def _save(self) -> None:
        self._data_dir.mkdir(parents=True, exist_ok=True)
        p = self._data_dir / "camera_registrations.json"
        tmp = p.with_suffix(".tmp")
        data = {nid: r.to_dict() for nid, r in self._registrations.items()}
        tmp.write_text(json.dumps(data, indent=2))
        tmp.chmod(0o600)
        tmp.rename(p)

    def _load(self) -> None:
        p = self._data_dir / "camera_registrations.json"
        if not p.exists():
            return
        try:
            data = json.loads(p.read_text())
            for nid, d in data.items():
                self._registrations[nid] = CameraRegistration.from_dict(d)
            log.debug("Loaded %d camera registrations from disk", len(self._registrations))
        except Exception:
            log.exception("Failed to load camera Connect state")
