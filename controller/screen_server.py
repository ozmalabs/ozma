# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
WebSocket server for native-rendering screen devices.

ESP32, Android apps, and dedicated ozma display endpoints connect here
to receive UI definitions and real-time data updates.

Protocol:

  Client → Server:
    {"type": "register", "device_id": "esp32-desk", "width": 320, "height": 240,
     "capabilities": ["gauge", "bar", "vu_meter", "label", "sparkline"]}

  Server → Client (once, on connect or layout change):
    {"type": "layout", "layout": { ... ScreenLayout JSON ... }}

  Server → Client (at refresh_hz):
    {"type": "data", "d": {"cpu_temp": 65.2, "ram_pct": 72, "scenario_name": "Gaming"}}

  Server → Client (on scenario switch):
    {"type": "scenario", "id": "gaming", "name": "Gaming", "color": "#E04040"}

  Client → Server (optional capability report):
    {"type": "capabilities", "widgets": ["gauge", "bar", "vu_meter", "label"]}

The server runs alongside the main FastAPI app on a separate port (7391).
"""

from __future__ import annotations

import asyncio
import json
import logging
from typing import Any

log = logging.getLogger("ozma.screen_server")

SCREEN_WS_PORT = 7391


class ScreenWebSocketServer:
    """WebSocket server for native-rendering screen devices."""

    def __init__(self, screen_manager: Any) -> None:
        self._screen_mgr = screen_manager
        self._server: Any = None
        self._clients: dict[str, Any] = {}  # device_id → (ws, Screen)

    async def start(self) -> None:
        try:
            import websockets
            self._server = await websockets.serve(
                self._handle_client, "0.0.0.0", SCREEN_WS_PORT
            )
            log.info("Screen WebSocket server listening on port %d", SCREEN_WS_PORT)
        except ImportError:
            # Fallback: use aiohttp or skip
            log.info("websockets library not installed — native screen server using asyncio fallback")
            await self._start_asyncio_server()

    async def _start_asyncio_server(self) -> None:
        """Minimal WebSocket server using raw asyncio (no external deps)."""
        self._server = await asyncio.start_server(
            self._handle_raw_connection, "0.0.0.0", SCREEN_WS_PORT
        )
        log.info("Screen TCP server on port %d (raw protocol)", SCREEN_WS_PORT)

    async def stop(self) -> None:
        if self._server:
            self._server.close()

    async def _handle_client(self, ws: Any, path: str = "") -> None:
        """Handle a WebSocket client connection."""
        device_id = ""
        try:
            async for message in ws:
                msg = json.loads(message)
                msg_type = msg.get("type", "")

                if msg_type == "register":
                    device_id = msg.get("device_id", f"device-{id(ws)}")
                    width = msg.get("width", 320)
                    height = msg.get("height", 240)
                    capabilities = msg.get("capabilities", [])

                    log.info("Native screen connected: %s (%dx%d, caps: %s)",
                             device_id, width, height, capabilities)

                    # Create a NativeRenderDriver and register the screen
                    from screen_manager import Screen, NativeRenderDriver
                    driver = NativeRenderDriver(device_id)
                    driver.set_websocket(ws)

                    screen = Screen(
                        id=device_id,
                        name=msg.get("name", device_id),
                        width=width,
                        height=height,
                        driver=driver,
                        layout_id=msg.get("layout", "panel-status"),
                        refresh_hz=msg.get("refresh_hz", 10),
                    )
                    self._screen_mgr.register_screen(screen)
                    self._clients[device_id] = (ws, screen)

                    # Push initial layout
                    if screen.layout:
                        await driver.push_layout(screen.layout.to_dict())

                    # Acknowledge
                    await ws.send(json.dumps({
                        "type": "registered",
                        "device_id": device_id,
                        "layout_id": screen.layout_id,
                    }))

                elif msg_type == "capabilities":
                    log.debug("Device %s capabilities: %s", device_id, msg.get("widgets", []))

        except Exception as e:
            log.debug("Screen client %s disconnected: %s", device_id or "unknown", e)
        finally:
            if device_id:
                self._screen_mgr.unregister_screen(device_id)
                self._clients.pop(device_id, None)
                log.info("Native screen disconnected: %s", device_id)

    async def _handle_raw_connection(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        """Fallback: simple newline-delimited JSON over TCP."""
        device_id = ""
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                msg = json.loads(line)
                if msg.get("type") == "register":
                    device_id = msg.get("device_id", "")
                    log.info("Native screen (TCP): %s", device_id)
                    # Simplified — no bidirectional push in TCP fallback
        except Exception:
            pass
        finally:
            writer.close()

    async def broadcast_scenario(self, scenario: dict) -> None:
        """Push scenario change to all connected native devices."""
        msg = json.dumps({"type": "scenario", **scenario})
        for device_id, (ws, screen) in list(self._clients.items()):
            try:
                await ws.send(msg)
            except Exception:
                pass
