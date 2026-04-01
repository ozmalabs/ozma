"""
Serial console capture on the node — catch what HDMI can't.

The node has a serial connection to its target machine (either via the
SBC's UART pins or a USB-serial adapter).  This captures kernel panics,
boot messages, and crash dumps that never reach the HDMI output.

The node buffers serial output locally and serves it via HTTP.  The
controller polls or streams it alongside the HDMI capture and HID.

Hardware:
  - SBC UART pins (TX/RX) → target machine's serial header (3-wire)
  - Or USB-serial adapter on the node's USB host port
  - Baud rate: typically 115200 (configurable)

Target setup:
  Linux:   Add console=ttyS0,115200 to kernel cmdline
  FreeBSD: console="comconsole" in /boot/loader.conf
  GRUB:    serial --unit=0 --speed=115200; terminal_output serial

Node HTTP API:
  GET  /serial/buffer          → last N lines of serial output
  GET  /serial/buffer?lines=50 → specific line count
  GET  /serial/stream          → SSE (Server-Sent Events) live stream
  POST /serial/send            → send text to the serial port
  GET  /serial/state           → connection state + buffer size

mDNS advertisement:
  cap=serial
  serial_port=/dev/ttyS1
  serial_baud=115200
"""

from __future__ import annotations

import asyncio
import logging
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from aiohttp import web

log = logging.getLogger("ozma.node.serial")

DEFAULT_BAUD = 115200
BUFFER_LINES = 5000


@dataclass
class SerialLine:
    timestamp: float
    text: str


class NodeSerialCapture:
    """
    Captures serial console output from the target machine.

    Runs on the node, serves buffered output via HTTP to the controller.
    """

    def __init__(
        self,
        port: str = "",
        baud: int = DEFAULT_BAUD,
    ) -> None:
        self._port = port
        self._baud = baud
        self._buffer: deque[SerialLine] = deque(maxlen=BUFFER_LINES)
        self._connected = False
        self._task: asyncio.Task | None = None
        self._subscribers: list[asyncio.Queue] = []

    @property
    def connected(self) -> bool:
        return self._connected

    @property
    def port(self) -> str:
        return self._port

    async def start(self) -> bool:
        """Start serial capture. Auto-detects port if not specified."""
        if not self._port:
            self._port = self._detect_port()
        if not self._port:
            log.info("No serial port found — serial capture disabled")
            return False

        self._task = asyncio.create_task(self._capture_loop(), name="serial-capture")
        log.info("Serial capture started: %s @ %d baud", self._port, self._baud)
        return True

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    def get_buffer(self, lines: int = 200) -> list[dict[str, Any]]:
        recent = list(self._buffer)[-lines:]
        return [{"ts": l.timestamp, "text": l.text} for l in recent]

    def get_text(self, lines: int = 200) -> str:
        return "\n".join(l.text for l in list(self._buffer)[-lines:])

    def subscribe(self) -> asyncio.Queue:
        """Subscribe to live serial output (for SSE streaming)."""
        q: asyncio.Queue = asyncio.Queue(maxsize=100)
        self._subscribers.append(q)
        return q

    def unsubscribe(self, q: asyncio.Queue) -> None:
        if q in self._subscribers:
            self._subscribers.remove(q)

    async def send(self, text: str) -> bool:
        """Send text to the serial port."""
        if not self._connected:
            return False
        try:
            # Write directly to the serial device
            loop = asyncio.get_running_loop()
            def _write():
                with open(self._port, "wb") as f:
                    f.write(text.encode())
                    f.flush()
            await loop.run_in_executor(None, _write)
            return True
        except Exception as e:
            log.debug("Serial send failed: %s", e)
            return False

    def state_dict(self) -> dict[str, Any]:
        return {
            "connected": self._connected,
            "port": self._port,
            "baud": self._baud,
            "buffer_lines": len(self._buffer),
        }

    # ── Capture loop ─────────────────────────────────────────────────────────

    async def _capture_loop(self) -> None:
        """Read serial port continuously."""
        while True:
            try:
                # Try pyserial-asyncio first
                try:
                    import serial_asyncio
                    reader, _ = await serial_asyncio.open_serial_connection(
                        url=self._port, baudrate=self._baud,
                    )
                    self._connected = True
                    log.info("Serial connected (asyncio): %s", self._port)
                    await self._read_lines(reader)
                except ImportError:
                    # Fallback: configure and read raw device
                    await self._configure_port()
                    self._connected = True
                    await self._read_device()

            except asyncio.CancelledError:
                return
            except Exception as e:
                log.debug("Serial error on %s: %s", self._port, e)
                self._connected = False
                await asyncio.sleep(5.0)

    async def _read_lines(self, reader: Any) -> None:
        """Read from pyserial-asyncio StreamReader."""
        while True:
            raw = await reader.readline()
            if not raw:
                break
            self._emit_line(raw.decode(errors="replace").rstrip())

    async def _read_device(self) -> None:
        """Read from raw device file."""
        loop = asyncio.get_running_loop()
        fd = open(self._port, "rb", buffering=0)
        try:
            buf = b""
            while True:
                data = await loop.run_in_executor(None, fd.read, 1024)
                if not data:
                    await asyncio.sleep(0.1)
                    continue
                buf += data
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    self._emit_line(line.decode(errors="replace").rstrip())
        finally:
            fd.close()

    async def _configure_port(self) -> None:
        """Set baud rate and raw mode on the serial device."""
        import subprocess
        try:
            subprocess.run(
                ["stty", "-F", self._port, str(self._baud), "raw", "-echo"],
                timeout=5, capture_output=True,
            )
        except Exception:
            pass

    def _emit_line(self, text: str) -> None:
        if not text.strip():
            return
        line = SerialLine(timestamp=time.time(), text=text)
        self._buffer.append(line)
        # Push to subscribers
        for q in list(self._subscribers):
            try:
                q.put_nowait(line)
            except asyncio.QueueFull:
                pass

    # ── Port detection ───────────────────────────────────────────────────────

    def _detect_port(self) -> str:
        """Auto-detect a serial port on the node.

        Priority:
          1. /dev/ttyGS0 — USB gadget ACM serial (built into the composite gadget,
             zero extra hardware, target sees it as /dev/ttyACM0)
          2. /dev/ttyS1, /dev/ttyS0 — SBC hardware UART
          3. /dev/ttyAMA0 — Raspberry Pi UART
          4. /dev/ttyUSB0 — USB-serial adapter
        """
        candidates = [
            # USB gadget serial (ACM) — preferred, zero cost, already in the USB cable
            "/dev/ttyGS0",
            # SBC UART (Raspberry Pi, Milk-V, etc.)
            "/dev/ttyS1", "/dev/ttyS0", "/dev/ttyAMA0",
            # USB-serial adapters
            "/dev/ttyUSB0", "/dev/ttyACM0",
        ]
        for port in candidates:
            if Path(port).exists():
                return port
        return ""


# ── HTTP route registration ──────────────────────────────────────────────────

def register_serial_routes(app: web.Application, serial: NodeSerialCapture) -> None:

    async def get_state(_: web.Request) -> web.Response:
        return web.json_response(serial.state_dict())

    async def get_buffer(request: web.Request) -> web.Response:
        lines = int(request.query.get("lines", "200"))
        return web.json_response(serial.get_buffer(lines))

    async def get_text(request: web.Request) -> web.Response:
        lines = int(request.query.get("lines", "200"))
        return web.Response(text=serial.get_text(lines), content_type="text/plain")

    async def post_send(request: web.Request) -> web.Response:
        body = await request.json()
        ok = await serial.send(body.get("text", ""))
        return web.json_response({"ok": ok})

    async def stream_sse(request: web.Request) -> web.StreamResponse:
        """Server-Sent Events stream of serial output."""
        resp = web.StreamResponse()
        resp.content_type = "text/event-stream"
        resp.headers["Cache-Control"] = "no-cache"
        resp.headers["Connection"] = "keep-alive"
        await resp.prepare(request)

        q = serial.subscribe()
        try:
            while True:
                line = await q.get()
                data = f"data: {line.text}\n\n"
                await resp.write(data.encode())
        except (asyncio.CancelledError, ConnectionResetError):
            pass
        finally:
            serial.unsubscribe(q)
        return resp

    async def get_setup_script(_: web.Request) -> web.Response:
        """Serve the enable_serial_console.sh script for the target machine.
        Usage on target: curl -sL http://ozma-node:7382/setup/serial | sudo sh
        """
        script_path = Path(__file__).parent.parent / "tinynode" / "gadget" / "enable_serial_console.sh"
        if script_path.exists():
            return web.Response(text=script_path.read_text(), content_type="text/x-shellscript")
        return web.Response(text="#!/bin/sh\necho 'Script not found'\nexit 1", content_type="text/x-shellscript")

    app.router.add_get("/serial/state", get_state)
    app.router.add_get("/serial/buffer", get_buffer)
    app.router.add_get("/serial/text", get_text)
    app.router.add_post("/serial/send", post_send)
    app.router.add_get("/serial/stream", stream_sse)
    app.router.add_get("/setup/serial", get_setup_script)
