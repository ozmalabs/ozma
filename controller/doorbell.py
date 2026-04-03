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
import logging
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

    async def receive_button_press(self, camera: str) -> AlertSession | None:
        """Doorbell button pressed at camera. Creates an urgent doorbell alert."""
        if not self._alert_mgr:
            return None
        snapshot = f"{self._frigate_url}/api/{camera}/latest.jpg"
        return await self._alert_mgr.create(
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

        snapshot = f"{self._frigate_url}/api/{camera}/latest.jpg"
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

        Phase 2: requires RTSP URL from camera config.
        Currently a stub — audio bridge classes are wired but not activated
        until camera RTSP URLs are stored in the node/service registry.
        """
        log.info("Doorbell audio answer: alert=%s (two-way audio Phase 2)", alert_id)

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
