# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Async QMP (QEMU Machine Protocol) clients — dual-socket design.

Two independent clients for two independent QMP sockets:

  QMPInputClient  — dedicated to input-send-event (keyboard + mouse)
    Fire-and-forget writes. No response reading. No locks. No races.
    This is the high-frequency path (~100+ events/sec).

  QMPControlClient — power, status, USB attach/detach, screendump
    Request/response with proper serialisation. Low frequency (<1/sec).
    Has a reader task that drains events and queues command responses.

Both auto-reconnect on disconnect with exponential backoff.

QEMU command line for dual sockets:
  -qmp unix:/tmp/vm-ctrl.qmp,server,nowait
  -qmp unix:/tmp/vm-input.qmp,server,nowait

Backward compatible: if only one socket path is given, QMPClient wraps
both roles into a single connection (legacy mode).
"""

import asyncio
import json
import logging
from pathlib import Path

log = logging.getLogger("ozma.softnode.qmp")

_BACKOFF_INITIAL = 0.5
_BACKOFF_MAX = 5.0
_BACKOFF_FACTOR = 2.0


# ── Input client (fire-and-forget, no reader) ─────────────────────────────────

class QMPInputClient:
    """
    Dedicated QMP client for input-send-event only.

    Writes keyboard/mouse events and never reads responses. This eliminates
    the reader/writer race condition that plagued the single-socket design.
    QEMU sends {"return":{}} for each event but we don't consume them —
    they accumulate in the kernel socket buffer and get discarded on close.
    """

    def __init__(self, socket_path: str) -> None:
        self._path = socket_path
        self._writer: asyncio.StreamWriter | None = None
        self._connected = False

    async def start(self) -> None:
        asyncio.create_task(self._connect_loop(), name="qmp-input-connect")

    async def stop(self) -> None:
        self._connected = False
        if self._writer:
            try:
                self._writer.close()
            except Exception:
                pass

    @property
    def connected(self) -> bool:
        return self._connected

    async def send_input_events(self, events: list[dict]) -> bool:
        if not events or not self._connected or not self._writer:
            return False
        try:
            cmd = {"execute": "input-send-event", "arguments": {"events": events}}
            self._writer.write(json.dumps(cmd).encode() + b"\n")
            await self._writer.drain()
            return True
        except (OSError, ConnectionResetError):
            self._connected = False
            asyncio.create_task(self._connect_loop(), name="qmp-input-reconnect")
            return False

    async def _connect_loop(self) -> None:
        backoff = _BACKOFF_INITIAL
        while True:
            if await self._connect():
                return
            await asyncio.sleep(backoff)
            backoff = min(backoff * _BACKOFF_FACTOR, _BACKOFF_MAX)

    async def _connect(self) -> bool:
        if not Path(self._path).exists():
            return False
        try:
            reader, writer = await asyncio.open_unix_connection(self._path)
            # Greeting
            line = await asyncio.wait_for(reader.readline(), timeout=3.0)
            if b"QMP" not in line:
                writer.close()
                return False
            # Capabilities
            writer.write(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
            await writer.drain()
            await asyncio.wait_for(reader.readline(), timeout=3.0)

            self._writer = writer
            self._connected = True

            # Start a background task to drain responses (prevents buffer buildup)
            asyncio.create_task(self._drain(reader), name="qmp-input-drain")
            log.info("QMP input connected: %s", self._path)
            return True
        except Exception as e:
            log.debug("QMP input connect failed: %s", e)
            return False

    async def _drain(self, reader: asyncio.StreamReader) -> None:
        """Silently consume all responses until EOF."""
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
        except Exception:
            pass
        if self._connected:
            self._connected = False
            log.info("QMP input disconnected, reconnecting")
            asyncio.create_task(self._connect_loop(), name="qmp-input-reconnect")


# ── Control client (request/response) ─────────────────────────────────────────

class QMPControlClient:
    """
    QMP client for control commands: power, status, USB, screendump.

    Uses proper request/response serialisation: one command at a time,
    wait for the response before sending the next. Low frequency path.
    """

    def __init__(self, socket_path: str) -> None:
        self._path = socket_path
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._connected = False
        self._lock = asyncio.Lock()

    async def start(self) -> None:
        asyncio.create_task(self._connect_loop(), name="qmp-ctrl-connect")

    async def stop(self) -> None:
        self._connected = False
        if self._writer:
            try:
                self._writer.close()
            except Exception:
                pass

    @property
    def connected(self) -> bool:
        return self._connected

    async def send_command(self, cmd: dict) -> dict | None:
        """Send a command and return the response dict, or None on failure."""
        async with self._lock:
            if not self._connected or not self._writer or not self._reader:
                return None
            try:
                self._writer.write(json.dumps(cmd).encode() + b"\n")
                await self._writer.drain()
                # Read response — skip any async events (have "event" key)
                for _ in range(10):  # max 10 events before giving up
                    line = await asyncio.wait_for(self._reader.readline(), timeout=5.0)
                    if not line:
                        self._connected = False
                        return None
                    resp = json.loads(line)
                    if "return" in resp or "error" in resp:
                        return resp
                    # It's an async event — skip and read next
                return None
            except (asyncio.TimeoutError, OSError, json.JSONDecodeError) as e:
                log.warning("QMP control error: %s", e)
                self._connected = False
                asyncio.create_task(self._connect_loop(), name="qmp-ctrl-reconnect")
                return None

    # ── Convenience methods ───────────────────────────────────────────

    async def system_powerdown(self) -> bool:
        resp = await self.send_command({"execute": "system_powerdown"})
        return resp is not None and "return" in resp

    async def system_reset(self) -> bool:
        resp = await self.send_command({"execute": "system_reset"})
        return resp is not None and "return" in resp

    async def pause(self) -> bool:
        resp = await self.send_command({"execute": "stop"})
        return resp is not None and "return" in resp

    async def cont(self) -> bool:
        resp = await self.send_command({"execute": "cont"})
        return resp is not None and "return" in resp

    async def query_status(self) -> dict | None:
        resp = await self.send_command({"execute": "query-status"})
        return resp.get("return") if resp else None

    async def screendump(self, output_path: str) -> bool:
        resp = await self.send_command({
            "execute": "screendump",
            "arguments": {"filename": output_path},
        })
        return resp is not None and "return" in resp

    async def attach_usb_storage(self, image_path: str, drive_id: str = "ozma-usb0",
                                  readonly: bool = False) -> bool:
        resp = await self.send_command({
            "execute": "blockdev-add",
            "arguments": {
                "driver": "file", "node-name": f"{drive_id}-file",
                "filename": image_path, "read-only": readonly,
            },
        })
        if not resp or "error" in resp:
            return False
        resp = await self.send_command({
            "execute": "device_add",
            "arguments": {
                "driver": "usb-storage", "id": drive_id,
                "drive": f"{drive_id}-file", "removable": True,
            },
        })
        return resp is not None and "return" in resp

    async def detach_usb_storage(self, drive_id: str = "ozma-usb0") -> bool:
        await self.send_command({"execute": "device_del", "arguments": {"id": drive_id}})
        await self.send_command({"execute": "blockdev-del", "arguments": {"node-name": f"{drive_id}-file"}})
        return True

    async def _connect_loop(self) -> None:
        backoff = _BACKOFF_INITIAL
        while True:
            if await self._connect():
                return
            await asyncio.sleep(backoff)
            backoff = min(backoff * _BACKOFF_FACTOR, _BACKOFF_MAX)

    async def _connect(self) -> bool:
        if not Path(self._path).exists():
            return False
        try:
            reader, writer = await asyncio.open_unix_connection(self._path)
            line = await asyncio.wait_for(reader.readline(), timeout=3.0)
            if b"QMP" not in line:
                writer.close()
                return False
            writer.write(json.dumps({"execute": "qmp_capabilities"}).encode() + b"\n")
            await writer.drain()
            resp_line = await asyncio.wait_for(reader.readline(), timeout=3.0)
            resp = json.loads(resp_line)
            if "return" not in resp:
                writer.close()
                return False

            self._reader = reader
            self._writer = writer
            self._connected = True
            log.info("QMP control connected: %s", self._path)
            return True
        except Exception as e:
            log.debug("QMP control connect failed: %s", e)
            return False


# ── Unified client (backward compatible) ──────────────────────────────────────

class QMPClient:
    """
    Unified QMP client — wraps input + control on separate sockets.

    If two socket paths are given, uses dedicated sockets (recommended).
    If only one path is given, falls back to single-socket mode (legacy).
    """

    def __init__(self, socket_path: str, input_socket_path: str = "") -> None:
        if input_socket_path:
            # Dual socket mode (recommended)
            self._ctrl = QMPControlClient(socket_path)
            self._input = QMPInputClient(input_socket_path)
            self._dual = True
        else:
            # Single socket mode (legacy — input and control share one connection)
            self._ctrl = QMPControlClient(socket_path)
            self._input = None
            self._dual = False

    async def start(self) -> None:
        await self._ctrl.start()
        if self._input:
            await self._input.start()

    async def stop(self) -> None:
        await self._ctrl.stop()
        if self._input:
            await self._input.stop()

    @property
    def connected(self) -> bool:
        if self._dual:
            return self._ctrl.connected and self._input.connected
        return self._ctrl.connected

    async def send_input_events(self, events: list[dict]) -> bool:
        if self._input:
            return await self._input.send_input_events(events)
        # Legacy: send through control client (not ideal but works)
        resp = await self._ctrl.send_command({
            "execute": "input-send-event",
            "arguments": {"events": events},
        })
        return resp is not None

    # Delegate control methods
    async def system_powerdown(self) -> bool:
        return await self._ctrl.system_powerdown()

    async def system_reset(self) -> bool:
        return await self._ctrl.system_reset()

    async def cont(self) -> bool:
        return await self._ctrl.cont()

    async def query_status(self) -> dict | None:
        return await self._ctrl.query_status()

    async def screendump(self, output_path: str) -> bool:
        return await self._ctrl.screendump(output_path)

    async def attach_usb_storage(self, image_path: str, drive_id: str = "ozma-usb0",
                                  readonly: bool = False) -> bool:
        return await self._ctrl.attach_usb_storage(image_path, drive_id, readonly)

    async def detach_usb_storage(self, drive_id: str = "ozma-usb0") -> bool:
        return await self._ctrl.detach_usb_storage(drive_id)
