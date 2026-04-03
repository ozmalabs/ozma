# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Doorbell adapter — translates Frigate camera events into AlertManager calls.

This module is responsible for doorbell-specific concerns only:
  - Interpreting Frigate MQTT events (button press vs person detection)
  - Two-way audio (CameraAudioBridge + VBANToBackchannelBridge, Phase 2)
  - Debouncing rapid re-triggers from the same camera

All session lifecycle, delivery (KDE Connect, notifications), expiry, and
WebSocket broadcasting is handled by AlertManager in alerts.py.

Event taxonomy from Frigate:
  frigate/<cam>/doorbell  payload=True   → button pressed → kind="doorbell"
                                            urgent; plays chime; Answer + Dismiss
  frigate/events  label=person            → person at door (no button press)
                  + sub_label             → recognised person
                                            passive; Dismiss only; no chime

The webhook in api.py calls receive_button_press() / receive_person_detected()
based on the MQTT-derived kind field.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import socket
import time
from typing import Any

from alerts import AlertManager, AlertSession

log = logging.getLogger("ozma.doorbell")

# Audio constants
_CAM_SAMPLE_RATE   = 48_000   # inbound (camera → headset): full quality
_CAM_CHANNELS      = 2
_VBAN_NODE_PORT    = 6980     # node VBAN receiver port (existing default)
_SAMPLES_PER_FRAME = 256      # matches vban.py DEFAULT_SAMPLES_PER_FRAME


# ── Doorbell-specific audio bridges ───────────────────────────────────────────

class CameraAudioBridge:
    """
    Pulls audio from a camera RTSP stream via ffmpeg and forwards it as
    VBAN UDP frames to the active node's headset output.

    Inbound path:
      ffmpeg → raw PCM (48kHz stereo) → Python VBAN packer → UDP → node:6980
      Node's existing VBANReceiver routes it through PipeWire to the headset.
    """

    def __init__(
        self,
        rtsp_url: str,
        node_host: str,
        node_port: int = _VBAN_NODE_PORT,
        stream_name: str = "doorbell-rx",
    ) -> None:
        self._rtsp_url = rtsp_url
        self._node_host = node_host
        self._node_port = node_port
        self._stream_name = stream_name
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        self._task = asyncio.create_task(self._run(), name="doorbell-cam-bridge")

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            self._proc.terminate()
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=3.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    async def _run(self) -> None:
        from vban import encode_header
        cmd = [
            "ffmpeg", "-loglevel", "error",
            "-rtsp_transport", "tcp",
            "-i", self._rtsp_url,
            "-vn",
            "-acodec", "pcm_s16le",
            "-ar", str(_CAM_SAMPLE_RATE),
            "-ac", str(_CAM_CHANNELS),
            "-f", "s16le", "-",
        ]
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            log.error("ffmpeg not found — doorbell inbound audio unavailable")
            return

        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        frame_bytes = _SAMPLES_PER_FRAME * _CAM_CHANNELS * 2
        counter = 0
        log.info("Doorbell inbound bridge: %s → VBAN → %s:%d",
                 self._rtsp_url, self._node_host, self._node_port)
        try:
            while True:
                assert self._proc.stdout is not None
                chunk = await self._proc.stdout.read(frame_bytes)
                if not chunk:
                    break
                if len(chunk) < frame_bytes:
                    chunk = chunk + b"\x00" * (frame_bytes - len(chunk))
                header = encode_header(
                    self._stream_name, counter,
                    _CAM_SAMPLE_RATE, _CAM_CHANNELS, _SAMPLES_PER_FRAME,
                )
                sock.sendto(header + chunk, (self._node_host, self._node_port))
                counter = (counter + 1) & 0xFFFF_FFFF
        except Exception as exc:
            log.debug("Doorbell inbound bridge ended: %s", exc)
        finally:
            sock.close()


class VBANToBackchannelBridge:
    """
    Receives VBAN frames from the active node's PipeWire mic (VBANSender)
    on a UDP port, strips the VBAN header, and forwards raw PCM to a
    camera RTSP backchannel via ffmpeg.

    Outbound path:
      Node PipeWire mic → VBANSender → UDP → controller:6982
      → VBANToBackchannelBridge → ffmpeg (G.711 µ-law) → camera RTSP backchannel

    The camera RTSP backchannel URL is camera-specific:
      Reolink:  rtsp://user:pass@camera-ip/backchannel
      Generic:  any RTSP ANNOUNCE/RECORD endpoint
    """

    PORT = 6982  # Controller listens here for mic VBAN from the active node

    def __init__(self, backchannel_url: str, listen_port: int = PORT) -> None:
        self._url = backchannel_url
        self._listen_port = listen_port
        self._proc: asyncio.subprocess.Process | None = None
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        cmd = [
            "ffmpeg", "-loglevel", "error",
            "-f", "s16le",
            "-ar", str(_CAM_SAMPLE_RATE),
            "-ac", str(_CAM_CHANNELS),
            "-i", "pipe:0",
            "-acodec", "pcm_mulaw",
            "-ar", "8000",
            "-ac", "1",
            "-f", "rtsp",
            "-rtsp_transport", "tcp",
            self._url,
        ]
        try:
            self._proc = await asyncio.create_subprocess_exec(
                *cmd,
                stdin=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
        except FileNotFoundError:
            log.error("ffmpeg not found — doorbell backchannel unavailable")
            return
        self._task = asyncio.create_task(
            self._receive_loop(), name="doorbell-vban-backchannel"
        )
        log.info("Doorbell backchannel: VBAN :%d → ffmpeg → %s",
                 self._listen_port, self._url)

    async def _receive_loop(self) -> None:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("0.0.0.0", self._listen_port))
        sock.setblocking(False)
        loop = asyncio.get_event_loop()
        try:
            while self._proc and self._proc.returncode is None:
                try:
                    data = await asyncio.wait_for(
                        loop.sock_recv(sock, 65535), timeout=1.0
                    )
                except asyncio.TimeoutError:
                    continue
                if len(data) <= 28:
                    continue
                pcm = data[28:]   # strip 28-byte VBAN header
                if self._proc.stdin:
                    self._proc.stdin.write(pcm)
                    await self._proc.stdin.drain()
        except Exception as exc:
            log.debug("Doorbell VBAN receive loop ended: %s", exc)
        finally:
            sock.close()

    async def stop(self) -> None:
        if self._proc and self._proc.returncode is None:
            if self._proc.stdin:
                try:
                    self._proc.stdin.close()
                except Exception:
                    pass
            try:
                await asyncio.wait_for(self._proc.wait(), timeout=3.0)
            except asyncio.TimeoutError:
                self._proc.kill()
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass


# ── Doorbell adapter ──────────────────────────────────────────────────────────

class DoorbellManager:
    """Translates Frigate doorbell events into AlertManager alerts.

    Doorbell button press → kind="doorbell" alert (urgent, chime, Answer + Dismiss)
    Person detected       → kind="motion"   alert (passive, no chime, Dismiss only)

    Audio bridges are managed here (doorbell-specific concern).
    All session lifecycle, delivery, expiry handled by AlertManager.
    """

    def __init__(
        self,
        state: Any,
        frigate_url: str = "http://localhost:5000",
        alert_mgr: AlertManager | None = None,
        # Kept for call-site compatibility — forwarded to AlertManager if provided
        kdeconnect: Any = None,
        notifier: Any = None,
    ) -> None:
        self._state = state
        self._frigate_url = frigate_url
        self._alert_mgr = alert_mgr
        # Audio state per alert-id
        self._audio: dict[str, tuple[CameraAudioBridge | None, VBANToBackchannelBridge | None]] = {}
        # Camera RTSP config from OZMA_DOORBELL_CAMERAS env var.
        # JSON dict: {"front_door": {"rtsp_inbound": "rtsp://...", "backchannel": "rtsp://..."}}
        self._camera_configs: dict[str, dict[str, str]] = self._load_camera_configs()

    @staticmethod
    def _load_camera_configs() -> dict[str, dict[str, str]]:
        """Load static camera configs from OZMA_DOORBELL_CAMERAS env var (fallback/override)."""
        raw = os.environ.get("OZMA_DOORBELL_CAMERAS", "")
        if not raw:
            return {}
        try:
            cfg = json.loads(raw)
            if isinstance(cfg, dict):
                return cfg
        except json.JSONDecodeError as exc:
            log.warning("OZMA_DOORBELL_CAMERAS parse error: %s", exc)
        return {}

    def _get_camera_configs(self) -> dict[str, dict[str, str]]:
        """Return merged camera configs: registered camera nodes take priority, env var fills gaps.

        Camera nodes registered with machine_class='camera' publish their streams
        in camera_streams. Each stream entry with a 'name' field becomes a camera
        config entry. The env var OZMA_DOORBELL_CAMERAS provides a static fallback
        for cameras not registered as nodes (e.g. third-party cameras).
        """
        configs: dict[str, dict[str, str]] = dict(self._camera_configs)  # start from env var
        nodes = getattr(self._state, "nodes", {})
        for node in nodes.values():
            if getattr(node, "machine_class", "") != "camera":
                continue
            frigate_host = getattr(node, "frigate_host", None) or node.host
            frigate_port = getattr(node, "frigate_port", None) or 5000
            for stream in getattr(node, "camera_streams", []):
                name = stream.get("name")
                if not name:
                    continue
                entry: dict[str, str] = {}
                if stream.get("rtsp_inbound"):
                    entry["rtsp_inbound"] = stream["rtsp_inbound"]
                if stream.get("backchannel"):
                    entry["backchannel"] = stream["backchannel"]
                if stream.get("hls"):
                    entry["hls"] = stream["hls"]
                # Derive snapshot URL from co-located Frigate if not explicitly provided
                if "snapshot_url" not in entry:
                    entry["snapshot_url"] = f"http://{frigate_host}:{frigate_port}/api/{name}/latest.jpg"
                configs[name] = entry
        return configs

    async def start(self) -> None:
        log.info("DoorbellManager started (frigate=%s)", self._frigate_url)

    async def stop(self) -> None:
        for cam_bridge, backchannel in self._audio.values():
            if cam_bridge:
                await cam_bridge.stop()
            if backchannel:
                await backchannel.stop()
        self._audio.clear()

    # ── Called from api.py webhook ────────────────────────────────────────────

    def _snapshot_url(self, camera: str) -> str:
        """Get the snapshot URL for a camera — from its node config or the fallback Frigate URL."""
        cam_cfg = self._get_camera_configs().get(camera, {})
        if cam_cfg.get("snapshot_url"):
            return cam_cfg["snapshot_url"]
        return f"{self._frigate_url}/api/{camera}/latest.jpg"

    async def receive_button_press(self, camera: str) -> AlertSession | None:
        """Doorbell button pressed at camera. Creates an urgent doorbell alert."""
        if not self._alert_mgr:
            return None
        snapshot = self._snapshot_url(camera)
        session = await self._alert_mgr.create(
            kind="doorbell",
            title="Doorbell",
            body=f"Someone at your door ({camera})",
            timeout_s=30,
            node_id=getattr(self._state, "active_node_id", None),
            snapshot_url=snapshot,
            camera=camera,
            primary_label="Answer",
            secondary_label="Dismiss",
            debounce_key=camera,
            debounce_s=5,
        )
        if session and camera in self._get_camera_configs():
            self._alert_mgr.register_acknowledge_callback(session.id, self.start_audio)
        return session

    async def receive_person_detected(
        self, camera: str, person: str = ""
    ) -> AlertSession | None:
        """Person detected at camera (with or without facial recognition).

        If a doorbell alert is already active on this camera, enrich it with
        the person name rather than creating a separate alert.
        """
        if not self._alert_mgr:
            return None

        # Enrich existing doorbell alert if present
        if person:
            existing = self._alert_mgr.get_most_recent_active(kind="doorbell")
            if existing and existing.camera == camera and not existing.person:
                await self._alert_mgr.update(existing.id, person=person,
                                             body=f"{person} at your door ({camera})")
                return existing

        snapshot = self._snapshot_url(camera)
        title = f"{person} at {camera}" if person else f"Person at {camera}"
        body = title
        return await self._alert_mgr.create(
            kind="motion",
            title=title,
            body=body,
            timeout_s=60,
            node_id=getattr(self._state, "active_node_id", None),
            snapshot_url=snapshot,
            camera=camera,
            person=person,
            primary_label="Dismiss",
            secondary_label="",
            debounce_key=camera,
            debounce_s=30,
        )

    # ── Called from ControlManager (alert.acknowledge → answer the door) ─────

    async def start_audio(self, alert_id: str) -> None:
        """Start two-way audio when the doorbell is answered.

        Inbound:  camera RTSP → ffmpeg → VBAN → active node:6980
                  (node's existing VBANReceiver routes it to the headset)
        Outbound: active node PipeWire mic → VBANSender → controller:6982
                  → VBANToBackchannelBridge → ffmpeg G.711 → camera RTSP backchannel
                  (the agent on the active node runs the VBANSender side)

        Requires OZMA_DOORBELL_CAMERAS to be configured with RTSP URLs.
        If the camera has no config entry, audio is skipped silently.
        """
        alert = self._alert_mgr.get_alert(alert_id) if self._alert_mgr else None
        if not alert:
            return

        cam_cfg = self._get_camera_configs().get(alert.camera, {})
        if not cam_cfg:
            log.debug("Doorbell audio: no camera config for %r — skipping", alert.camera)
            return

        rtsp_inbound = cam_cfg.get("rtsp_inbound", "")
        backchannel_url = cam_cfg.get("backchannel", "")

        active_node = getattr(self._state, "active_node_id", None)
        node_host = self._resolve_node_host(active_node)

        cam_bridge: CameraAudioBridge | None = None
        backchannel: VBANToBackchannelBridge | None = None

        if rtsp_inbound and node_host:
            cam_bridge = CameraAudioBridge(
                rtsp_url=rtsp_inbound,
                node_host=node_host,
            )
            await cam_bridge.start()

        if backchannel_url:
            backchannel = VBANToBackchannelBridge(backchannel_url=backchannel_url)
            await backchannel.start()

        self._audio[alert_id] = (cam_bridge, backchannel)
        log.info(
            "Doorbell audio started: alert=%s camera=%s node=%s inbound=%s backchannel=%s",
            alert_id, alert.camera, node_host,
            "yes" if cam_bridge else "no",
            "yes" if backchannel else "no",
        )

    def _resolve_node_host(self, node_id: str | None) -> str | None:
        """Look up the hostname/IP of the active node from AppState."""
        if not node_id:
            return None
        nodes = getattr(self._state, "nodes", {})
        node = nodes.get(node_id)
        if node is None:
            return None
        # NodeInfo has host and/or address attributes
        host = getattr(node, "host", None) or getattr(node, "address", None)
        return host if host else None

    async def stop_audio(self, alert_id: str) -> None:
        bridges = self._audio.pop(alert_id, (None, None))
        for b in bridges:
            if b:
                await b.stop()

    # ── Backwards-compat: old call sites that use DoorbellManager directly ───

    async def receive_event(self, camera: str, event_type: str, payload: Any) -> AlertSession | None:
        """Compatibility shim — route to receive_button_press()."""
        return await self.receive_button_press(camera)

    def enrich_person(self, camera: str, person: str) -> None:
        """Compatibility shim — enrich via AlertManager."""
        if not self._alert_mgr:
            return
        existing = self._alert_mgr.get_most_recent_active(kind="doorbell")
        if existing and existing.camera == camera and not existing.person:
            asyncio.create_task(
                self._alert_mgr.update(existing.id, person=person,
                                       body=f"{person} at your door ({camera})"),
                name=f"doorbell-enrich-{existing.id}",
            )

    def get_session(self, alert_id: str) -> Any:
        return self._alert_mgr.get_alert(alert_id) if self._alert_mgr else None

    def get_sessions(self) -> list[dict]:
        return self._alert_mgr.list_alerts(kind="doorbell") if self._alert_mgr else []

    def get_snapshot_url(self, alert_id: str) -> str | None:
        return self._alert_mgr.get_snapshot_url(alert_id) if self._alert_mgr else None
