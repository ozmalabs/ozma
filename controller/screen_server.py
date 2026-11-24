# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
WebSocket server for native-rendering screen devices.

ESP32, Android apps, and dedicated ozma display endpoints connect here
to receive UI definitions and real-time data updates.

JSON protocol (legacy default):

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

Protobuf protocol (opt-in):

  Registration is still JSON but includes ``"protocol": "protobuf"`` (or the
  alias ``"wire_format": "protobuf"``)::

    {"type": "register", "device_id": "esp32-desk", "protocol": "protobuf"}

  The server then sends one binary ``[tag][protobuf]`` DisplayDefinition frame
  followed by compact DataUpdate frames (see ``display_protocol``).  Devices
  may send binary ActionInvoke and InteractionEvent frames back on the same
  WebSocket.

The server runs alongside the main FastAPI app on a separate port (7391).
"""

from __future__ import annotations

import asyncio
import inspect
import json
import logging
from typing import Any

log = logging.getLogger("ozma.screen_server")

SCREEN_WS_PORT = 7391


class ScreenWebSocketServer:
    """WebSocket server for native-rendering screen devices."""

    def __init__(
        self,
        screen_manager: Any,
        *,
        action_handler: Any = None,
        interaction_handler: Any = None,
    ) -> None:
        self._screen_mgr = screen_manager
        self._server: Any = None
        self._clients: dict[str, Any] = {}  # device_id → (ws, Screen)
        self._protocols: dict[str, str] = {}  # device_id → "json" | "protobuf"
        self._action_handler = action_handler
        self._interaction_handler = interaction_handler

    @staticmethod
    def _protocol_for(msg: dict) -> str:
        """Return the requested wire format, defaulting to JSON."""
        requested = msg.get("protocol", msg.get("wire_format", "json"))
        if requested is None:
            return "json"
        name = str(requested).strip().lower().replace("-", "_")
        if name in {"protobuf", "proto", "pb", "binary"}:
            return "protobuf"
        return "json"

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
                if isinstance(message, (bytes, bytearray, memoryview)):
                    raw = bytes(message)
                    # Some legacy clients send JSON registration frames as
                    # binary WebSocket messages.  Preserve that path; anything
                    # else is a protobuf Display frame.
                    if not raw.lstrip().startswith(b"{"):
                        await self._handle_binary_message(device_id, raw)
                        continue
                    message = raw

                try:
                    msg = json.loads(message)
                except (ValueError, TypeError):
                    log.debug("Ignoring malformed screen message from %s",
                              device_id or "unknown")
                    continue
                if not isinstance(msg, dict):
                    continue
                msg_type = msg.get("type", "")

                if msg_type == "register":
                    device_id = msg.get("device_id", f"device-{id(ws)}")
                    width = msg.get("width", 320)
                    height = msg.get("height", 240)
                    capabilities = msg.get("capabilities", [])
                    protocol = self._protocol_for(msg)

                    log.info("Native screen connected: %s (%dx%d, caps: %s, wire=%s)",
                             device_id, width, height, capabilities, protocol)

                    # Create the tier-2 driver for the requested wire format.
                    from screen_manager import Screen, NativeRenderDriver

                    if protocol == "protobuf":
                        from screen_drivers import NativeRenderProtobufDriver

                        driver = NativeRenderProtobufDriver(
                            device_id,
                            on_action=self._on_action,
                            on_interaction=self._on_interaction,
                        )
                    else:
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
                    self._protocols[device_id] = protocol

                    # Push initial layout (a DisplayDefinition frame for
                    # protobuf devices, legacy JSON for everyone else).
                    if screen.layout:
                        await driver.push_layout(screen.layout.to_dict())

                    # Acknowledge. Registration and the ack stay JSON even for
                    # protobuf clients so a device can negotiate with one parser.
                    await ws.send(json.dumps({
                        "type": "registered",
                        "device_id": device_id,
                        "layout_id": screen.layout_id,
                        "protocol": protocol,
                        "wire_format": protocol,
                    }))

                elif msg_type == "capabilities":
                    log.debug("Device %s capabilities: %s", device_id, msg.get("widgets", []))

        except Exception as e:
            log.debug("Screen client %s disconnected: %s", device_id or "unknown", e)
        finally:
            if device_id:
                self._screen_mgr.unregister_screen(device_id)
                unregister_driver = getattr(
                    self._screen_mgr, "unregister_native_driver", None
                )
                if unregister_driver is not None:
                    unregister_driver(device_id)
                self._clients.pop(device_id, None)
                self._protocols.pop(device_id, None)
                log.info("Native screen disconnected: %s", device_id)

    async def _handle_binary_message(self, device_id: str, frame: bytes) -> None:
        """Decode an inbound Display frame from a protobuf client."""
        if not device_id:
            log.debug("Ignoring binary screen frame received before registration")
            return
        client = self._clients.get(device_id)
        if client is None:
            return
        screen = client[1]
        handler = getattr(screen.driver, "handle_inbound_frame", None)
        if handler is None:
            log.debug("Ignoring binary frame from JSON client %s", device_id)
            return
        try:
            await handler(frame)
        except Exception as exc:
            log.debug("Rejected malformed display frame from %s: %s", device_id, exc)

    async def _on_action(self, device_id: str, invoke: Any) -> None:
        """Dispatch an inbound ActionInvoke to the controller."""
        handler = self._action_handler
        if handler is None:
            handler = getattr(self._screen_mgr, "handle_action_invoke", None)
        if handler is None:
            log.info("Action invoke from %s: action_id=%s",
                     device_id, getattr(invoke, "action_id", "?"))
            return
        result = handler(device_id, invoke)
        if inspect.isawaitable(result):
            await result

    async def _on_interaction(self, device_id: str, event: Any) -> None:
        """Dispatch an inbound InteractionEvent to the controller."""
        handler = self._interaction_handler
        if handler is None:
            handler = getattr(self._screen_mgr, "handle_interaction_event", None)
        if handler is None:
            log.debug("Interaction event from %s: node_id=%s",
                      device_id, getattr(event, "node_id", "?"))
            return
        result = handler(device_id, event)
        if inspect.isawaitable(result):
            await result

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
            if self._protocols.get(device_id) == "protobuf":
                # Protobuf devices receive scenario variables through typed
                # DataUpdate frames from the screen manager loop.
                continue
            try:
                await ws.send(msg)
            except Exception:
                pass
