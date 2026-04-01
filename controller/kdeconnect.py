# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
KDE Connect integration for ozma.

Speaks the KDE Connect protocol to integrate with Android phones (via
the KDE Connect app) and Linux desktops (via the KDE Connect daemon).
No custom companion app needed — KDE Connect is mature, open-source,
and available on all platforms.

What ozma gets from KDE Connect:

  Phone → Ozma:
    - Notifications → RGB note layer + dashboard display
    - Battery level → dashboard + low battery alert
    - Media player state → audio routing awareness
    - Clipboard → scenario-aware clipboard sharing
    - Phone ringer status → mute desk audio during calls
    - SMS → dashboard notification

  Ozma → Phone:
    - Media control (play/pause/next) → bound to control surfaces
    - Find my phone (ring it)
    - Clipboard push
    - Run command (custom automation)

KDE Connect protocol:
  Discovery: UDP broadcast on port 1716
    {"id":"<uuid>","name":"Ozma","type":"kdeconnect.identity",
     "protocolVersion":7,"incomingCapabilities":[...],
     "outgoingCapabilities":[...]}

  Connection: TCP on port 1716, then TLS upgrade
    Messages are newline-delimited JSON with type field

Supported plugins (capabilities):
  kdeconnect.battery           — battery level + charging state
  kdeconnect.notification      — receive phone notifications
  kdeconnect.clipboard         — bidirectional clipboard sync
  kdeconnect.mprisremote       — media player control
  kdeconnect.findmyphone       — ring the phone
  kdeconnect.runcommand        — execute commands on the phone
  kdeconnect.telephony         — call state (ringing, in call, idle)
  kdeconnect.ping              — simple ping/pong
  kdeconnect.share             — file/URL sharing
"""

from __future__ import annotations

import asyncio
import json
import logging
import socket
import ssl
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Callable, Coroutine

log = logging.getLogger("ozma.kdeconnect")

KDECONNECT_PORT = 1716
PROTOCOL_VERSION = 7

# Capabilities ozma advertises
INCOMING_CAPABILITIES = [
    "kdeconnect.battery",
    "kdeconnect.notification",
    "kdeconnect.clipboard",
    "kdeconnect.telephony",
    "kdeconnect.mprisremote",
    "kdeconnect.ping",
]

OUTGOING_CAPABILITIES = [
    "kdeconnect.clipboard",
    "kdeconnect.mprisremote",
    "kdeconnect.findmyphone",
    "kdeconnect.runcommand",
    "kdeconnect.ping",
]


@dataclass
class KDEDevice:
    """A discovered KDE Connect device (phone, desktop)."""

    device_id: str
    name: str
    device_type: str = "phone"       # "phone", "desktop", "tablet", "tv"
    paired: bool = False
    connected: bool = False
    host: str = ""
    port: int = KDECONNECT_PORT
    battery_level: int = -1          # -1 = unknown
    battery_charging: bool = False
    last_seen: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "device_id": self.device_id,
            "name": self.name,
            "device_type": self.device_type,
            "paired": self.paired,
            "connected": self.connected,
            "battery_level": self.battery_level,
            "battery_charging": self.battery_charging,
        }


# Event callback
EventCallback = Callable[[str, dict], Coroutine[Any, Any, None]]


class KDEConnectBridge:
    """
    Bridges KDE Connect devices with ozma's event system.

    Discovers phones/desktops on the network, receives notifications,
    battery state, and call state.  Forwards events to ozma's RGB
    compositor (notifications → notes), dashboard, and control system.

    Usage::

        kc = KDEConnectBridge()
        kc.on_event = my_callback
        await kc.start()

        # Phone notifications appear as RGB notes
        # Battery state shows on dashboard
        # Incoming call → mute desk audio
    """

    def __init__(self, device_name: str = "Ozma Controller") -> None:
        self._device_name = device_name
        self._device_id = str(uuid.uuid4()).replace("-", "")[:32]
        self._devices: dict[str, KDEDevice] = {}
        self._discovery_task: asyncio.Task | None = None
        self._listener_task: asyncio.Task | None = None
        self._connections: dict[str, asyncio.Task] = {}  # device_id → connection task
        self.on_event: EventCallback | None = None

    async def start(self) -> None:
        self._discovery_task = asyncio.create_task(
            self._discovery_loop(), name="kdeconnect-discovery"
        )
        self._listener_task = asyncio.create_task(
            self._listen_for_connections(), name="kdeconnect-listener"
        )
        log.info("KDE Connect bridge started (id: %s)", self._device_id[:8])

    async def stop(self) -> None:
        for task in [self._discovery_task, self._listener_task, *self._connections.values()]:
            if task:
                task.cancel()

    def list_devices(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._devices.values()]

    def get_device(self, device_id: str) -> KDEDevice | None:
        return self._devices.get(device_id)

    # ── Actions (ozma → phone) ───────────────────────────────────────────────

    async def find_my_phone(self, device_id: str) -> bool:
        """Ring the phone."""
        return await self._send_packet(device_id, {
            "type": "kdeconnect.findmyphone.request",
            "body": {},
        })

    async def send_clipboard(self, device_id: str, content: str) -> bool:
        """Push clipboard content to a device."""
        return await self._send_packet(device_id, {
            "type": "kdeconnect.clipboard",
            "body": {"content": content},
        })

    async def media_action(self, device_id: str, action: str) -> bool:
        """Send media control: play, pause, next, previous, stop."""
        return await self._send_packet(device_id, {
            "type": "kdeconnect.mprisremote.request",
            "body": {"action": action},
        })

    async def ping(self, device_id: str, message: str = "") -> bool:
        """Send a ping (shows notification on the device)."""
        body = {"message": message} if message else {}
        return await self._send_packet(device_id, {
            "type": "kdeconnect.ping",
            "body": body,
        })

    async def run_command(self, device_id: str, command_key: str) -> bool:
        """Trigger a pre-configured command on the device."""
        return await self._send_packet(device_id, {
            "type": "kdeconnect.runcommand.request",
            "body": {"key": command_key},
        })

    # ── Discovery ────────────────────────────────────────────────────────────

    def _identity_packet(self) -> bytes:
        """Build our identity broadcast packet."""
        packet = {
            "id": int(time.time() * 1000),
            "type": "kdeconnect.identity",
            "body": {
                "deviceId": self._device_id,
                "deviceName": self._device_name,
                "deviceType": "desktop",
                "protocolVersion": PROTOCOL_VERSION,
                "incomingCapabilities": INCOMING_CAPABILITIES,
                "outgoingCapabilities": OUTGOING_CAPABILITIES,
                "tcpPort": KDECONNECT_PORT,
            },
        }
        return (json.dumps(packet) + "\n").encode()

    async def _discovery_loop(self) -> None:
        """Broadcast identity and listen for responses."""
        while True:
            try:
                # Send identity broadcast
                sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
                sock.setblocking(False)
                try:
                    sock.sendto(self._identity_packet(), ("<broadcast>", KDECONNECT_PORT))
                except Exception:
                    pass
                sock.close()

                # Also listen for incoming identity broadcasts
                await asyncio.sleep(30.0)
            except asyncio.CancelledError:
                return
            except Exception:
                await asyncio.sleep(60.0)

    async def _listen_for_connections(self) -> None:
        """Listen for incoming TCP connections from KDE Connect devices."""
        try:
            server = await asyncio.start_server(
                self._handle_connection, "0.0.0.0", KDECONNECT_PORT,
            )
            async with server:
                await server.serve_forever()
        except asyncio.CancelledError:
            return
        except OSError as e:
            log.warning("KDE Connect listener failed (port %d): %s", KDECONNECT_PORT, e)

    async def _handle_connection(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        """Handle an incoming KDE Connect connection."""
        peer = writer.get_extra_info("peername")
        log.debug("KDE Connect connection from %s", peer)

        try:
            # First message should be identity
            line = await asyncio.wait_for(reader.readline(), timeout=10.0)
            packet = json.loads(line)

            if packet.get("type") != "kdeconnect.identity":
                writer.close()
                return

            body = packet.get("body", {})
            device_id = body.get("deviceId", "")
            device_name = body.get("deviceName", "Unknown")
            device_type = body.get("deviceType", "phone")

            device = KDEDevice(
                device_id=device_id,
                name=device_name,
                device_type=device_type,
                host=peer[0] if peer else "",
                port=body.get("tcpPort", KDECONNECT_PORT),
                connected=True,
                last_seen=time.monotonic(),
            )
            self._devices[device_id] = device
            log.info("KDE Connect device: %s (%s) from %s", device_name, device_type, peer)

            await self._emit("kdeconnect.device_connected", device.to_dict())

            # Send our identity back
            writer.write(self._identity_packet())
            await writer.drain()

            # Read messages
            while True:
                line = await reader.readline()
                if not line:
                    break
                await self._handle_packet(device, json.loads(line))

        except (asyncio.TimeoutError, json.JSONDecodeError, ConnectionResetError):
            pass
        finally:
            if device_id and device_id in self._devices:
                self._devices[device_id].connected = False
                await self._emit("kdeconnect.device_disconnected", {"device_id": device_id})
            writer.close()

    async def _handle_packet(self, device: KDEDevice, packet: dict) -> None:
        """Process an incoming KDE Connect packet."""
        ptype = packet.get("type", "")
        body = packet.get("body", {})

        match ptype:
            case "kdeconnect.battery":
                device.battery_level = body.get("currentCharge", -1)
                device.battery_charging = body.get("isCharging", False)
                await self._emit("kdeconnect.battery", {
                    "device_id": device.device_id,
                    "device_name": device.name,
                    "level": device.battery_level,
                    "charging": device.battery_charging,
                })

            case "kdeconnect.notification":
                await self._emit("kdeconnect.notification", {
                    "device_id": device.device_id,
                    "device_name": device.name,
                    "app": body.get("appName", ""),
                    "title": body.get("title", ""),
                    "text": body.get("text", ""),
                    "ticker": body.get("ticker", ""),
                    "is_clearable": body.get("isClearable", True),
                })

            case "kdeconnect.telephony":
                event = body.get("event", "")
                await self._emit("kdeconnect.telephony", {
                    "device_id": device.device_id,
                    "device_name": device.name,
                    "event": event,  # "ringing", "talking", "idle"
                    "phone_number": body.get("phoneNumber", ""),
                    "contact_name": body.get("contactName", ""),
                })

            case "kdeconnect.clipboard":
                await self._emit("kdeconnect.clipboard", {
                    "device_id": device.device_id,
                    "content": body.get("content", ""),
                })

            case "kdeconnect.ping":
                await self._emit("kdeconnect.ping", {
                    "device_id": device.device_id,
                    "device_name": device.name,
                    "message": body.get("message", ""),
                })

            case _:
                log.debug("KDE Connect unhandled: %s from %s", ptype, device.name)

    async def _send_packet(self, device_id: str, packet: dict) -> bool:
        """Send a packet to a connected device."""
        device = self._devices.get(device_id)
        if not device or not device.connected or not device.host:
            return False

        packet["id"] = int(time.time() * 1000)
        data = (json.dumps(packet) + "\n").encode()

        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_connection(device.host, device.port), timeout=5.0
            )
            # Send identity first
            writer.write(self._identity_packet())
            await writer.drain()
            # Then the actual packet
            writer.write(data)
            await writer.drain()
            writer.close()
            return True
        except Exception as e:
            log.debug("KDE Connect send to %s failed: %s", device.name, e)
            return False

    async def _emit(self, event_type: str, data: dict) -> None:
        if self.on_event:
            try:
                await self.on_event(event_type, data)
            except Exception:
                log.debug("KDE Connect event callback error", exc_info=True)
