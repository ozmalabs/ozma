# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Broadcast studio — OBS-backed scene management with touchscreen control.

Provides a broadcast-style production interface where sources (cameras,
capture cards, overlays, media, NDI streams) are arranged into scenes
that can be switched live.  OBS Studio runs as the compositing backend
via obs-websocket, while ozma provides the control layer.

Architecture:
  ┌─────────────────────────────────────────────────────────┐
  │                     ozma controller                      │
  │  ┌──────────────┐   ┌──────────────┐   ┌─────────────┐ │
  │  │ BroadcastMgr │──▶│ OBSConnector │──▶│  OBS Studio  ││
  │  │  (scenes,    │   │ (websocket)  │   │  (headless)  ││
  │  │   sources,   │   └──────────────┘   └─────────────┘ │
  │  │   control)   │                                       │
  │  └──────────────┘                                       │
  │        ▲                                                │
  │        │  REST API + WebSocket                          │
  │  ┌─────┴──────────────────────────────────────────┐     │
  │  │   Web UI / Touchscreen / Stream Deck / MIDI    │     │
  │  └────────────────────────────────────────────────┘     │
  └─────────────────────────────────────────────────────────┘

OBS integration:
  - obs-websocket v5 protocol (port 4455)
  - Scene creation, source management, transitions
  - Preview/Program (studio mode) support
  - Recording and streaming control
  - Audio mixer control per source

Sources available to the broadcast:
  - Ozma capture cards (hdmi-0, hdmi-1, ...)
  - Ozma camera feeds (local/RTSP/ONVIF cameras)
  - NDI inputs (auto-discovered)
  - Media files (video, image)
  - Browser sources (web dashboards, overlays)
  - Screen capture (from any connected display)
  - Text/graphics (lower thirds, scoreboard)
  - Ozma overlays (existing overlay_sources)

Touchscreen control:
  The broadcast UI is designed for touch interaction:
  - Large scene buttons with preview thumbnails
  - Drag-and-drop source positioning
  - Pinch-to-zoom source scaling
  - Swipe transitions between scenes
  - Audio mixer with touch faders

Control surface integration:
  BroadcastSurface registers with ControlManager:
  - Buttons → scene switch, transition, record, stream
  - Faders → audio mix, transition position (T-bar)
  - Displays → scene name, recording time, stream status
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.obs_studio")


# ── OBS WebSocket connector ────────────────────────────────────────────────

class OBSConnector:
    """
    Connects to OBS Studio via obs-websocket v5.

    Provides async methods for scene/source management, transitions,
    recording, and streaming.
    """

    def __init__(self, host: str = "127.0.0.1", port: int = 4455,
                 password: str = "") -> None:
        self._host = host
        self._port = port
        self._password = password
        self._ws: Any = None
        self._connected = False
        self._request_id = 0
        self._pending: dict[str, asyncio.Future] = {}
        self._recv_task: asyncio.Task | None = None
        self.on_event: Any = None  # Callback for OBS events

    @property
    def connected(self) -> bool:
        return self._connected

    async def connect(self) -> bool:
        """Connect to OBS WebSocket server."""
        try:
            import websockets
            url = f"ws://{self._host}:{self._port}"
            self._ws = await websockets.connect(url)

            # obs-websocket v5 handshake
            hello = json.loads(await self._ws.recv())
            if hello.get("op") != 0:
                log.warning("Unexpected OBS hello: %s", hello)
                return False

            # Authenticate if required
            auth_data: dict[str, Any] = {"rpcVersion": 1}
            auth = hello.get("d", {}).get("authentication")
            if auth and self._password:
                challenge = auth["challenge"]
                salt = auth["salt"]
                import base64
                secret = base64.b64encode(
                    hashlib.sha256((self._password + salt).encode()).digest()
                ).decode()
                auth_response = base64.b64encode(
                    hashlib.sha256((secret + challenge).encode()).digest()
                ).decode()
                auth_data["authentication"] = auth_response

            await self._ws.send(json.dumps({"op": 1, "d": auth_data}))
            identified = json.loads(await self._ws.recv())
            if identified.get("op") != 2:
                log.warning("OBS auth failed: %s", identified)
                return False

            self._connected = True
            self._recv_task = asyncio.create_task(self._receive_loop(), name="obs-recv")
            log.info("Connected to OBS at %s:%d", self._host, self._port)
            return True
        except Exception as e:
            log.debug("OBS connection failed: %s", e)
            return False

    async def disconnect(self) -> None:
        self._connected = False
        if self._recv_task:
            self._recv_task.cancel()
        if self._ws:
            try:
                await self._ws.close()
            except Exception:
                pass

    async def _receive_loop(self) -> None:
        try:
            async for msg in self._ws:
                data = json.loads(msg)
                op = data.get("op")
                d = data.get("d", {})
                if op == 5:  # Event
                    if self.on_event:
                        await self.on_event(d.get("eventType", ""), d.get("eventData", {}))
                elif op == 7:  # RequestResponse
                    req_id = d.get("requestId", "")
                    future = self._pending.pop(req_id, None)
                    if future and not future.done():
                        future.set_result(d)
        except asyncio.CancelledError:
            return
        except Exception as e:
            log.warning("OBS receive error: %s", e)
            self._connected = False

    async def _request(self, request_type: str, data: dict | None = None) -> dict:
        if not self._connected or not self._ws:
            return {"requestStatus": {"result": False, "comment": "Not connected"}}
        self._request_id += 1
        req_id = str(self._request_id)
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._pending[req_id] = future
        msg = {
            "op": 6,
            "d": {"requestType": request_type, "requestId": req_id,
                  "requestData": data or {}},
        }
        await self._ws.send(json.dumps(msg))
        try:
            return await asyncio.wait_for(future, timeout=10.0)
        except asyncio.TimeoutError:
            self._pending.pop(req_id, None)
            return {"requestStatus": {"result": False, "comment": "Timeout"}}

    # ── Scene operations ────────────────────────────────────────────────────

    async def get_scene_list(self) -> list[dict]:
        r = await self._request("GetSceneList")
        return r.get("responseData", {}).get("scenes", [])

    async def get_current_program_scene(self) -> str:
        r = await self._request("GetCurrentProgramScene")
        return r.get("responseData", {}).get("currentProgramSceneName", "")

    async def get_current_preview_scene(self) -> str:
        r = await self._request("GetCurrentPreviewScene")
        return r.get("responseData", {}).get("currentPreviewSceneName", "")

    async def set_current_program_scene(self, name: str) -> bool:
        r = await self._request("SetCurrentProgramScene", {"sceneName": name})
        return r.get("requestStatus", {}).get("result", False)

    async def set_current_preview_scene(self, name: str) -> bool:
        r = await self._request("SetCurrentPreviewScene", {"sceneName": name})
        return r.get("requestStatus", {}).get("result", False)

    async def create_scene(self, name: str) -> bool:
        r = await self._request("CreateScene", {"sceneName": name})
        return r.get("requestStatus", {}).get("result", False)

    async def remove_scene(self, name: str) -> bool:
        r = await self._request("RemoveScene", {"sceneName": name})
        return r.get("requestStatus", {}).get("result", False)

    # ── Source operations ───────────────────────────────────────────────────

    async def get_scene_items(self, scene_name: str) -> list[dict]:
        r = await self._request("GetSceneItemList", {"sceneName": scene_name})
        return r.get("responseData", {}).get("sceneItems", [])

    async def create_input(self, scene_name: str, input_name: str,
                           input_kind: str, settings: dict | None = None) -> int:
        r = await self._request("CreateInput", {
            "sceneName": scene_name, "inputName": input_name,
            "inputKind": input_kind, "inputSettings": settings or {},
        })
        return r.get("responseData", {}).get("sceneItemId", -1)

    async def remove_input(self, input_name: str) -> bool:
        r = await self._request("RemoveInput", {"inputName": input_name})
        return r.get("requestStatus", {}).get("result", False)

    async def set_input_settings(self, input_name: str, settings: dict) -> bool:
        r = await self._request("SetInputSettings", {
            "inputName": input_name, "inputSettings": settings,
        })
        return r.get("requestStatus", {}).get("result", False)

    # ── Transitions ─────────────────────────────────────────────────────────

    async def get_transition_list(self) -> list[dict]:
        r = await self._request("GetSceneTransitionList")
        return r.get("responseData", {}).get("transitions", [])

    async def set_current_transition(self, name: str) -> bool:
        r = await self._request("SetCurrentSceneTransition", {"transitionName": name})
        return r.get("requestStatus", {}).get("result", False)

    async def set_transition_duration(self, ms: int) -> bool:
        r = await self._request("SetCurrentSceneTransitionDuration", {"transitionDuration": ms})
        return r.get("requestStatus", {}).get("result", False)

    async def trigger_transition(self) -> bool:
        r = await self._request("TriggerStudioModeTransition")
        return r.get("requestStatus", {}).get("result", False)

    # ── Recording ───────────────────────────────────────────────────────────

    async def start_recording(self) -> bool:
        r = await self._request("StartRecord")
        return r.get("requestStatus", {}).get("result", False)

    async def stop_recording(self) -> str:
        r = await self._request("StopRecord")
        return r.get("responseData", {}).get("outputPath", "")

    async def get_record_status(self) -> dict:
        r = await self._request("GetRecordStatus")
        return r.get("responseData", {})

    # ── Streaming ───────────────────────────────────────────────────────────

    async def start_streaming(self) -> bool:
        r = await self._request("StartStream")
        return r.get("requestStatus", {}).get("result", False)

    async def stop_streaming(self) -> bool:
        r = await self._request("StopStream")
        return r.get("requestStatus", {}).get("result", False)

    async def get_stream_status(self) -> dict:
        r = await self._request("GetStreamStatus")
        return r.get("responseData", {})

    # ── Audio ───────────────────────────────────────────────────────────────

    async def get_input_volume(self, name: str) -> dict:
        r = await self._request("GetInputVolume", {"inputName": name})
        return r.get("responseData", {})

    async def set_input_volume(self, name: str, volume_db: float) -> bool:
        r = await self._request("SetInputVolume", {"inputName": name, "inputVolumeDb": volume_db})
        return r.get("requestStatus", {}).get("result", False)

    async def set_input_mute(self, name: str, muted: bool) -> bool:
        r = await self._request("SetInputMute", {"inputName": name, "inputMuted": muted})
        return r.get("requestStatus", {}).get("result", False)

    # ── Studio mode ─────────────────────────────────────────────────────────

    async def get_studio_mode(self) -> bool:
        r = await self._request("GetStudioModeEnabled")
        return r.get("responseData", {}).get("studioModeEnabled", False)

    async def set_studio_mode(self, enabled: bool) -> bool:
        r = await self._request("SetStudioModeEnabled", {"studioModeEnabled": enabled})
        return r.get("requestStatus", {}).get("result", False)

    # ── Screenshots ─────────────────────────────────────────────────────────

    async def get_source_screenshot(self, name: str, fmt: str = "png",
                                     width: int = 320) -> str:
        r = await self._request("GetSourceScreenshot", {
            "sourceName": name, "imageFormat": fmt, "imageWidth": width,
        })
        return r.get("responseData", {}).get("imageData", "")


# ── Broadcast scene model ──────────────────────────────────────────────────

@dataclass
class BroadcastSource:
    """A source available for use in broadcast scenes."""
    id: str
    name: str
    source_type: str       # camera, capture, ndi, media, browser, text, overlay, screen
    input_kind: str = ""   # OBS input kind (e.g., "ffmpeg_source", "browser_source")
    settings: dict = field(default_factory=dict)
    obs_input_name: str = ""

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name, "type": self.source_type,
            "input_kind": self.input_kind, "obs_name": self.obs_input_name,
        }


@dataclass
class BroadcastScene:
    """A broadcast scene with positioned sources."""
    id: str
    name: str
    obs_scene_name: str = ""
    sources: list[dict] = field(default_factory=list)
    transition: str = "Cut"
    transition_ms: int = 300

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "obs_scene_name": self.obs_scene_name,
            "sources": self.sources,
            "transition": self.transition,
            "transition_ms": self.transition_ms,
        }


# ── Broadcast manager ──────────────────────────────────────────────────────

class OBSStudioManager:
    """
    Manages broadcast production — scenes, sources, transitions, recording.

    Connects to OBS Studio for compositing and provides a simplified
    production API suitable for touchscreens and control surfaces.
    """

    def __init__(self, obs_host: str = "127.0.0.1", obs_port: int = 4455,
                 obs_password: str = "") -> None:
        self._obs = OBSConnector(obs_host, obs_port, obs_password)
        self._scenes: dict[str, BroadcastScene] = {}
        self._sources: dict[str, BroadcastSource] = {}
        self._program_scene: str = ""
        self._preview_scene: str = ""
        self._recording = False
        self._streaming = False
        self._config_path = Path(__file__).parent / "obs_scenes.json"
        self._reconnect_task: asyncio.Task | None = None
        self._load_config()

    def _load_config(self) -> None:
        if self._config_path.exists():
            try:
                data = json.loads(self._config_path.read_text())
                for s in data.get("scenes", []):
                    scene = BroadcastScene(**{k: v for k, v in s.items()
                                             if k in BroadcastScene.__dataclass_fields__})
                    self._scenes[scene.id] = scene
                for s in data.get("sources", []):
                    src = BroadcastSource(**{k: v for k, v in s.items()
                                            if k in BroadcastSource.__dataclass_fields__})
                    self._sources[src.id] = src
            except Exception as e:
                log.warning("Failed to load OBS studio config: %s", e)

    def _save_config(self) -> None:
        data = {
            "scenes": [s.to_dict() for s in self._scenes.values()],
            "sources": [s.to_dict() for s in self._sources.values()],
        }
        self._config_path.write_text(json.dumps(data, indent=2))

    async def start(self) -> None:
        """Connect to OBS and sync state."""
        connected = await self._obs.connect()
        if connected:
            self._obs.on_event = self._on_obs_event
            await self._sync_from_obs()
        else:
            log.info("OBS not available — broadcast studio in offline mode")
            self._reconnect_task = asyncio.create_task(
                self._reconnect_loop(), name="obs-reconnect"
            )

    async def stop(self) -> None:
        if self._reconnect_task:
            self._reconnect_task.cancel()
        await self._obs.disconnect()
        self._save_config()

    async def _reconnect_loop(self) -> None:
        while True:
            await asyncio.sleep(10)
            if not self._obs.connected:
                if await self._obs.connect():
                    self._obs.on_event = self._on_obs_event
                    await self._sync_from_obs()
                    log.info("OBS reconnected")
                    return

    async def _sync_from_obs(self) -> None:
        self._program_scene = await self._obs.get_current_program_scene()
        try:
            self._preview_scene = await self._obs.get_current_preview_scene()
        except Exception:
            self._preview_scene = ""
        record_status = await self._obs.get_record_status()
        self._recording = record_status.get("outputActive", False)
        stream_status = await self._obs.get_stream_status()
        self._streaming = stream_status.get("outputActive", False)

    async def _on_obs_event(self, event_type: str, data: dict) -> None:
        match event_type:
            case "CurrentProgramSceneChanged":
                self._program_scene = data.get("sceneName", "")
            case "CurrentPreviewSceneChanged":
                self._preview_scene = data.get("sceneName", "")
            case "RecordStateChanged":
                self._recording = data.get("outputActive", False)
            case "StreamStateChanged":
                self._streaming = data.get("outputActive", False)

    # ── Scene management ────────────────────────────────────────────────────

    async def create_scene(self, scene_id: str, name: str) -> BroadcastScene | None:
        obs_name = f"ozma-{scene_id}"
        if self._obs.connected:
            if not await self._obs.create_scene(obs_name):
                return None
        scene = BroadcastScene(id=scene_id, name=name, obs_scene_name=obs_name)
        self._scenes[scene_id] = scene
        self._save_config()
        return scene

    async def remove_scene(self, scene_id: str) -> bool:
        scene = self._scenes.get(scene_id)
        if not scene:
            return False
        if self._obs.connected and scene.obs_scene_name:
            await self._obs.remove_scene(scene.obs_scene_name)
        del self._scenes[scene_id]
        self._save_config()
        return True

    async def switch_scene(self, scene_id: str, preview: bool = False) -> bool:
        scene = self._scenes.get(scene_id)
        if not scene or not self._obs.connected:
            return False
        if preview:
            return await self._obs.set_current_preview_scene(scene.obs_scene_name)
        return await self._obs.set_current_program_scene(scene.obs_scene_name)

    async def trigger_transition(self) -> bool:
        if not self._obs.connected:
            return False
        return await self._obs.trigger_transition()

    # ── Source management ───────────────────────────────────────────────────

    def register_source(self, source: BroadcastSource) -> None:
        self._sources[source.id] = source
        self._save_config()

    def register_ozma_sources(self, captures: Any = None, cameras: Any = None) -> None:
        """Auto-register ozma capture cards and cameras as broadcast sources."""
        if captures:
            for src in captures.list_sources():
                self._sources[f"capture-{src['id']}"] = BroadcastSource(
                    id=f"capture-{src['id']}",
                    name=f"Capture: {src.get('name', src['id'])}",
                    source_type="capture",
                    input_kind="ffmpeg_source",
                    settings={"local_file": f"http://localhost:7380/captures/{src['id']}/stream.m3u8",
                              "is_local_file": False},
                )
        if cameras:
            for cam in cameras.list_cameras():
                if cam.get("active"):
                    self._sources[f"camera-{cam['id']}"] = BroadcastSource(
                        id=f"camera-{cam['id']}",
                        name=f"Camera: {cam.get('name', cam['id'])}",
                        source_type="camera",
                        input_kind="ffmpeg_source",
                        settings={"local_file": f"http://localhost:7380/cameras/{cam['id']}/stream.m3u8",
                                  "is_local_file": False},
                    )

    async def add_source_to_scene(self, scene_id: str, source_id: str,
                                   x: int = 0, y: int = 0,
                                   width: int = 1920, height: int = 1080) -> bool:
        scene = self._scenes.get(scene_id)
        source = self._sources.get(source_id)
        if not scene or not source:
            return False
        if self._obs.connected and scene.obs_scene_name:
            obs_name = f"ozma-{source_id}"
            item_id = await self._obs.create_input(
                scene.obs_scene_name, obs_name, source.input_kind, source.settings,
            )
            if item_id < 0:
                return False
            source.obs_input_name = obs_name
        scene.sources.append({
            "source_id": source_id, "x": x, "y": y,
            "width": width, "height": height, "visible": True,
        })
        self._save_config()
        return True

    # ── Recording/streaming ─────────────────────────────────────────────────

    async def start_recording(self) -> bool:
        return await self._obs.start_recording() if self._obs.connected else False

    async def stop_recording(self) -> str:
        return await self._obs.stop_recording() if self._obs.connected else ""

    async def start_streaming(self) -> bool:
        return await self._obs.start_streaming() if self._obs.connected else False

    async def stop_streaming(self) -> bool:
        return await self._obs.stop_streaming() if self._obs.connected else False

    # ── Audio mixing ────────────────────────────────────────────────────────

    async def set_source_volume(self, source_id: str, volume_db: float) -> bool:
        source = self._sources.get(source_id)
        if not source or not source.obs_input_name or not self._obs.connected:
            return False
        return await self._obs.set_input_volume(source.obs_input_name, volume_db)

    async def set_source_mute(self, source_id: str, muted: bool) -> bool:
        source = self._sources.get(source_id)
        if not source or not source.obs_input_name or not self._obs.connected:
            return False
        return await self._obs.set_input_mute(source.obs_input_name, muted)

    # ── Thumbnails ──────────────────────────────────────────────────────────

    async def get_scene_thumbnail(self, scene_id: str, width: int = 320) -> str:
        scene = self._scenes.get(scene_id)
        if not scene or not self._obs.connected:
            return ""
        return await self._obs.get_source_screenshot(scene.obs_scene_name, width=width)

    # ── Status ──────────────────────────────────────────────────────────────

    def status(self) -> dict[str, Any]:
        return {
            "connected": self._obs.connected,
            "program_scene": self._program_scene,
            "preview_scene": self._preview_scene,
            "recording": self._recording,
            "streaming": self._streaming,
            "scenes": [s.to_dict() for s in self._scenes.values()],
            "sources": [s.to_dict() for s in self._sources.values()],
        }

    def list_scenes(self) -> list[dict]:
        return [s.to_dict() for s in self._scenes.values()]

    def list_sources(self) -> list[dict]:
        return [s.to_dict() for s in self._sources.values()]
