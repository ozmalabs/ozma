"""
Serial console capture — kernel messages, boot logs, and crash dumps.

Connects to target machines via serial port (RS-232, USB-serial, IPMI SOL)
to capture console output that never appears on the display:

  - Kernel panics (the full backtrace, not just the screen freeze)
  - Boot messages (dmesg, GRUB, initramfs)
  - Kernel oops and warnings
  - Filesystem errors that happen before X/Wayland starts
  - FreeBSD/OpenBSD/NetBSD console output
  - Hypervisor serial consoles (QEMU -serial, ESXi DCUI)
  - Network switch/router CLI (Cisco IOS, JunOS, etc.)
  - Embedded device debug output

Why serial matters:
  HDMI capture + OCR can read the screen, but many critical messages
  never reach the screen:
  - Kernel panics that freeze the GPU before updating the framebuffer
  - Early boot messages before the display driver loads
  - Messages that scroll past too fast to OCR
  - Headless servers with no display at all
  - Serial-only devices (routers, switches, SBCs without HDMI)

Serial capture runs continuously, buffering all output.  The OCR trigger
system scans the serial buffer alongside the display for error patterns.

Hardware:
  - USB-serial adapter (FTDI, CP2102, CH340) on the controller or node
  - Null-modem cable to the target machine's serial port (DB9 or header)
  - Many servers have serial headers on the motherboard (COM1)
  - IPMI Serial-over-LAN (SOL) for remote serial access

Configuration:
  Per-node serial config in scenarios.json or controls.yaml:
    {"serial": {"port": "/dev/ttyUSB0", "baud": 115200}}
  Or auto-detected from node mDNS: serial_port=/dev/ttyUSB0

Target machine setup:
  Linux:  console=ttyS0,115200 in kernel cmdline (GRUB)
  FreeBSD: console="comconsole" in /boot/loader.conf
  Windows: bcdedit /dbgsettings serial debugport:1 baudrate:115200
"""

from __future__ import annotations

import asyncio
import logging
import re
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.serial_console")

DEFAULT_BAUD = 115200
BUFFER_LINES = 10000     # Keep last 10K lines per console
SCROLL_BACK = 500        # Lines visible in the dashboard


@dataclass
class SerialLine:
    """A single line from the serial console with timestamp."""
    timestamp: float
    text: str
    severity: str = "info"   # info, warning, error, critical


@dataclass
class SerialConsole:
    """A serial console connection to a target machine."""

    id: str                  # e.g., "node-1-serial"
    node_id: str = ""        # Associated ozma node
    port: str = ""           # /dev/ttyUSB0, /dev/ttyS0, etc.
    baud: int = DEFAULT_BAUD
    connected: bool = False
    buffer: deque[SerialLine] = field(default_factory=lambda: deque(maxlen=BUFFER_LINES))
    _proc: asyncio.subprocess.Process | None = field(default=None, repr=False)
    _reader_task: asyncio.Task | None = field(default=None, repr=False)

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "node_id": self.node_id,
            "port": self.port,
            "baud": self.baud,
            "connected": self.connected,
            "buffer_lines": len(self.buffer),
        }

    def get_recent(self, lines: int = SCROLL_BACK) -> list[dict[str, Any]]:
        recent = list(self.buffer)[-lines:]
        return [{"ts": l.timestamp, "text": l.text, "severity": l.severity} for l in recent]

    def get_text(self, lines: int = SCROLL_BACK) -> str:
        return "\n".join(l.text for l in list(self.buffer)[-lines:])


# ── Severity detection for serial output ─────────────────────────────────────

_SEVERITY_PATTERNS: list[tuple[str, str]] = [
    # Critical
    (r"Kernel panic", "critical"),
    (r"BUG:|Oops:", "critical"),
    (r"Hardware Error|Machine check", "critical"),
    (r"RIP:|Call Trace:", "critical"),
    (r"not syncing", "critical"),

    # Error
    (r"\berror\b", "error"),
    (r"\bfailed\b", "error"),
    (r"I/O error", "error"),
    (r"segfault", "error"),
    (r"Out of memory", "error"),
    (r"readonly", "error"),

    # Warning
    (r"\bwarn", "warning"),
    (r"deprecated", "warning"),
    (r"timed out", "warning"),

    # Info (default)
]

_COMPILED_PATTERNS = [(re.compile(p, re.IGNORECASE), sev) for p, sev in _SEVERITY_PATTERNS]


def _classify_severity(text: str) -> str:
    for pattern, severity in _COMPILED_PATTERNS:
        if pattern.search(text):
            return severity
    return "info"


class SerialConsoleManager:
    """
    Manages serial console connections to all target machines.

    Opens serial ports, captures output continuously, classifies
    message severity, and feeds the OCR trigger system.
    """

    def __init__(self, state: Any = None) -> None:
        self._state = state
        self._consoles: dict[str, SerialConsole] = {}
        self._scan_task: asyncio.Task | None = None
        self.on_line: Any = None      # async callback(console_id, line: SerialLine)
        self.on_alert: Any = None     # async callback(console_id, severity, text)

    async def start(self) -> None:
        self._scan_task = asyncio.create_task(self._scan_loop(), name="serial-scan")
        log.info("Serial console manager started")

    async def stop(self) -> None:
        if self._scan_task:
            self._scan_task.cancel()
        for console in self._consoles.values():
            await self._disconnect(console)

    def add_console(self, console_id: str, port: str, baud: int = DEFAULT_BAUD,
                    node_id: str = "") -> SerialConsole:
        """Manually add a serial console."""
        console = SerialConsole(id=console_id, node_id=node_id, port=port, baud=baud)
        self._consoles[console_id] = console
        return console

    def list_consoles(self) -> list[dict[str, Any]]:
        return [c.to_dict() for c in self._consoles.values()]

    def get_console(self, console_id: str) -> SerialConsole | None:
        return self._consoles.get(console_id)

    def get_output(self, console_id: str, lines: int = SCROLL_BACK) -> list[dict[str, Any]]:
        c = self._consoles.get(console_id)
        return c.get_recent(lines) if c else []

    def get_text(self, console_id: str, lines: int = SCROLL_BACK) -> str:
        c = self._consoles.get(console_id)
        return c.get_text(lines) if c else ""

    async def send(self, console_id: str, text: str) -> bool:
        """Send text to a serial console (for interactive use)."""
        c = self._consoles.get(console_id)
        if not c or not c._proc or not c._proc.stdin:
            return False
        try:
            c._proc.stdin.write(text.encode())
            await c._proc.stdin.drain()
            return True
        except Exception:
            return False

    # ── Connection management ────────────────────────────────────────────────

    async def _connect(self, console: SerialConsole) -> bool:
        """Open a serial port connection."""
        if not Path(console.port).exists():
            return False

        try:
            # Use picocom/screen/minicom for robust serial handling,
            # or direct asyncio serial if pyserial-asyncio is available
            try:
                import serial_asyncio
                reader, writer = await serial_asyncio.open_serial_connection(
                    url=console.port, baudrate=console.baud,
                )
                console.connected = True
                console._reader_task = asyncio.create_task(
                    self._read_serial_async(console, reader),
                    name=f"serial-{console.id}",
                )
                log.info("Serial connected (asyncio): %s @ %d baud", console.port, console.baud)
                return True
            except ImportError:
                pass

            # Fallback: use cat on the device (works on Linux)
            proc = await asyncio.create_subprocess_exec(
                "stty", "-F", console.port, str(console.baud), "raw", "-echo",
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await proc.wait()

            proc = await asyncio.create_subprocess_exec(
                "cat", console.port,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL,
            )
            console._proc = proc
            console.connected = True
            console._reader_task = asyncio.create_task(
                self._read_proc(console, proc),
                name=f"serial-{console.id}",
            )
            log.info("Serial connected (cat): %s @ %d baud", console.port, console.baud)
            return True

        except Exception as e:
            log.debug("Serial connect failed %s: %s", console.port, e)
            return False

    async def _disconnect(self, console: SerialConsole) -> None:
        console.connected = False
        if console._reader_task:
            console._reader_task.cancel()
        if console._proc and console._proc.returncode is None:
            console._proc.terminate()

    async def _read_serial_async(self, console: SerialConsole, reader: Any) -> None:
        """Read from pyserial-asyncio reader."""
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                await self._process_line(console, line.decode(errors="replace").rstrip())
        except asyncio.CancelledError:
            return
        except Exception:
            pass
        console.connected = False

    async def _read_proc(self, console: SerialConsole, proc: asyncio.subprocess.Process) -> None:
        """Read from cat subprocess."""
        try:
            buf = ""
            while True:
                data = await proc.stdout.read(4096)
                if not data:
                    break
                buf += data.decode(errors="replace")
                while "\n" in buf:
                    line, buf = buf.split("\n", 1)
                    await self._process_line(console, line.rstrip())
        except asyncio.CancelledError:
            return
        except Exception:
            pass
        console.connected = False

    async def _process_line(self, console: SerialConsole, text: str) -> None:
        """Process a line of serial output."""
        if not text.strip():
            return

        severity = _classify_severity(text)
        line = SerialLine(timestamp=time.time(), text=text, severity=severity)
        console.buffer.append(line)

        # Fire callbacks
        if self.on_line:
            try:
                await self.on_line(console.id, line)
            except Exception:
                pass

        if severity in ("error", "critical") and self.on_alert:
            try:
                await self.on_alert(console.id, severity, text)
            except Exception:
                pass

    # ── Auto-discovery ───────────────────────────────────────────────────────

    async def _scan_loop(self) -> None:
        """Scan for serial ports, connect local, and pull from remote nodes."""
        while True:
            try:
                # Connect any configured but disconnected local consoles
                for console in self._consoles.values():
                    if not console.connected and console.port and not console.port.startswith("remote:"):
                        await self._connect(console)

                # Auto-detect USB-serial adapters on the controller
                for dev in sorted(Path("/dev").glob("ttyUSB*")):
                    dev_id = f"auto-{dev.name}"
                    if dev_id not in self._consoles:
                        console = SerialConsole(
                            id=dev_id, port=str(dev), baud=DEFAULT_BAUD,
                        )
                        self._consoles[dev_id] = console
                        log.info("Auto-detected local serial port: %s", dev)

                # Pull serial output from nodes that have serial capture (cap=serial)
                if self._state:
                    await self._poll_node_serial()

                await asyncio.sleep(10.0)
            except asyncio.CancelledError:
                return

    async def _poll_node_serial(self) -> None:
        """Pull serial buffers from nodes advertising cap=serial."""
        import urllib.request
        import json as _json

        for node in list(self._state.nodes.values()):
            if "serial" not in getattr(node, "capabilities", []):
                continue
            if not node.api_port:
                continue

            console_id = f"node-{node.id.split('.')[0]}-serial"
            if console_id not in self._consoles:
                self._consoles[console_id] = SerialConsole(
                    id=console_id, node_id=node.id,
                    port=f"remote:{node.host}:{node.api_port}",
                )
                log.info("Node serial source: %s via %s:%d", node.id, node.host, node.api_port)

            console = self._consoles[console_id]
            try:
                loop = asyncio.get_running_loop()
                url = f"http://{node.host}:{node.api_port}/serial/buffer?lines=50"
                def _fetch(u=url):
                    with urllib.request.urlopen(u, timeout=3) as r:
                        return _json.loads(r.read())
                lines = await loop.run_in_executor(None, _fetch)

                last_ts = console.buffer[-1].timestamp if console.buffer else 0
                for entry in lines:
                    ts = entry.get("ts", time.time())
                    if ts > last_ts:
                        text = entry.get("text", "")
                        severity = _classify_severity(text)
                        sl = SerialLine(timestamp=ts, text=text, severity=severity)
                        console.buffer.append(sl)

                        if severity in ("error", "critical") and self.on_alert:
                            await self.on_alert(console_id, severity, text)

                console.connected = True
            except Exception:
                console.connected = False
