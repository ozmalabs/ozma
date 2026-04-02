# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Terminal↔screen bridge using VGA bitmap OCR.

Turns any node's display into a real terminal session — no video codec required.

Architecture:
  - OCR polls the display at ~30fps via bitmap template matching (0.2ms/frame)
  - Changed cells are emitted as ANSI true-color escape sequences to xterm.js
  - Keyboard input arrives as JSON (same format as remote_desktop.py) → HID packets
  - Session transport: single WebSocket carries both ANSI output and key input

Frame sources (tried in order):
  1. Node HTTP /display/snapshot — soft nodes use QMP screendump → JPEG
  2. Controller stream snapshot — V4L2/ffmpeg capture pipeline
  3. Direct QMP screendump (if state has qmp_clients mapping)

Use cases: BIOS setup, UEFI shell, GRUB menu, Linux console, DOS, serial terminals,
boot sequences before any OS loads, kernel panic screens, installer text UI.
"""

from __future__ import annotations

import asyncio
import io
import json
import logging
import time
from dataclasses import dataclass, field
from typing import Any, Callable, Awaitable

log = logging.getLogger("ozma.terminal_bridge")

try:
    from PIL import Image as _PILImage
    import numpy as np
    _DEPS_AVAILABLE = True
except ImportError:
    _DEPS_AVAILABLE = False
    log.debug("PIL/numpy not available — terminal bridge disabled")

from text_capture import TextCapture, OCRResult


# ── Cell state ────────────────────────────────────────────────────────────────

@dataclass(frozen=True)
class CellState:
    char: str
    fg: tuple[int, int, int]   # RGB foreground
    bg: tuple[int, int, int]   # RGB background


_BLANK = CellState(" ", (204, 204, 204), (0, 0, 0))

# ANSI palette for nearest-color fallback when cells haven't changed colors
_ANSI256 = None  # built lazily


def _build_ansi256() -> list[tuple[int, int, int]]:
    """Build the xterm 256-color palette for nearest-color lookup."""
    palette: list[tuple[int, int, int]] = []
    # Standard 16 colors (0-15)
    for r, g, b in [
        (0,0,0),(128,0,0),(0,128,0),(128,128,0),(0,0,128),(128,0,128),(0,128,128),(192,192,192),
        (128,128,128),(255,0,0),(0,255,0),(255,255,0),(0,0,255),(255,0,255),(0,255,255),(255,255,255),
    ]:
        palette.append((r, g, b))
    # 6×6×6 color cube (16-231)
    for r in range(6):
        for g in range(6):
            for b in range(6):
                palette.append((0 if r == 0 else 55 + r * 40,
                                 0 if g == 0 else 55 + g * 40,
                                 0 if b == 0 else 55 + b * 40))
    # Grayscale ramp (232-255)
    for i in range(24):
        v = 8 + i * 10
        palette.append((v, v, v))
    return palette


def _ansi_colors_changed(prev: CellState | None, curr: CellState) -> bool:
    if prev is None:
        return True
    return prev.fg != curr.fg or prev.bg != curr.bg


# ── ANSI generation ───────────────────────────────────────────────────────────

def _ansi_true_color(fg: tuple[int,int,int], bg: tuple[int,int,int]) -> str:
    return (f"\x1b[38;2;{fg[0]};{fg[1]};{fg[2]}m"
            f"\x1b[48;2;{bg[0]};{bg[1]};{bg[2]}m")


def full_repaint(cells: list[list[CellState]]) -> bytes:
    """Generate ANSI escape bytes for a full screen repaint."""
    out: list[str] = ["\x1b[2J\x1b[H\x1b[?25l"]   # clear, home, hide cursor

    for r, row in enumerate(cells):
        if not row:
            continue
        # Move to start of row
        out.append(f"\x1b[{r + 1};1H")
        cur_fg: tuple | None = None
        cur_bg: tuple | None = None

        for cell in row:
            if cell.fg != cur_fg or cell.bg != cur_bg:
                out.append(_ansi_true_color(cell.fg, cell.bg))
                cur_fg, cur_bg = cell.fg, cell.bg
            out.append(cell.char)

    out.append("\x1b[0m")
    return "".join(out).encode("utf-8", errors="replace")


def diff_repaint(prev: list[list[CellState]], curr: list[list[CellState]]) -> bytes:
    """Generate minimal ANSI diff between two screen states."""
    out: list[str] = ["\x1b[?25l"]  # hide cursor during update

    for r, row in enumerate(curr):
        prev_row = prev[r] if r < len(prev) else []
        cur_fg: tuple | None = None
        cur_bg: tuple | None = None
        last_col = -2  # last column we wrote

        for c, cell in enumerate(row):
            prev_cell = prev_row[c] if c < len(prev_row) else None
            if prev_cell == cell:
                continue

            # Move cursor if not consecutive
            if c != last_col + 1:
                out.append(f"\x1b[{r + 1};{c + 1}H")
            last_col = c

            if cell.fg != cur_fg or cell.bg != cur_bg:
                out.append(_ansi_true_color(cell.fg, cell.bg))
                cur_fg, cur_bg = cell.fg, cell.bg

            out.append(cell.char)

    out.append("\x1b[0m")
    if len(out) == 2:
        return b""  # nothing changed
    return "".join(out).encode("utf-8", errors="replace")


# ── Color extraction ──────────────────────────────────────────────────────────

def _extract_colors(
    frame_rgb: "np.ndarray",
    row: int, col: int,
    cell_w: int, cell_h: int,
    glyph_mask: "np.ndarray | None",
) -> tuple[tuple[int,int,int], tuple[int,int,int]]:
    """Extract foreground and background RGB for one character cell."""
    y = row * cell_h
    x = col * cell_w
    patch = frame_rgb[y:y + cell_h, x:x + cell_w]   # (H, W, 3)

    if patch.size == 0:
        return (204, 204, 204), (0, 0, 0)

    if glyph_mask is not None and glyph_mask.shape == (patch.shape[0], patch.shape[1]):
        lit = glyph_mask > 0.5
        fg_pixels = patch[lit]
        bg_pixels = patch[~lit]
        fg = tuple(int(v) for v in fg_pixels.mean(axis=0)) if len(fg_pixels) else (204, 204, 204)
        bg = tuple(int(v) for v in bg_pixels.mean(axis=0)) if len(bg_pixels) else (0, 0, 0)
    else:
        # Space or unknown: bg = average color, fg = light grey
        bg_arr = patch.mean(axis=(0, 1))
        bg = tuple(int(v) for v in bg_arr)
        fg = (204, 204, 204)

    return fg, bg   # type: ignore[return-value]


def _build_cell_grid(
    frame_rgb: "np.ndarray",
    result: OCRResult,
    fonts: Any,
) -> list[list[CellState]]:
    """Build a 2D grid of CellState from an OCR result + color sampling."""
    cw, ch = result.cell_width, result.cell_height
    grid: list[list[CellState]] = []

    font = fonts[0] if fonts else None

    for r, row_cells in enumerate(result.cells):
        row: list[CellState] = []
        for c, cell_info in enumerate(row_cells):
            char = cell_info.get("char", " ") or " "
            code = cell_info.get("code", 32)

            # Retrieve matched glyph mask for color separation
            mask: "np.ndarray | None" = None
            if font is not None and code in font.glyphs:
                raw_mask = font.glyphs[code]
                # Resize to actual cell dimensions if needed
                if raw_mask.shape != (ch, cw):
                    from text_capture import TextCapture as _TC
                    mask = _TC._resize(raw_mask, cw, ch)
                else:
                    mask = raw_mask

            fg, bg = _extract_colors(frame_rgb, r, c, cw, ch, mask)
            row.append(CellState(char=char, fg=fg, bg=bg))
        grid.append(row)

    return grid


# ── Cursor detection ──────────────────────────────────────────────────────────

def _estimate_cursor(prev: list[list[CellState]] | None,
                     curr: list[list[CellState]]) -> tuple[int, int] | None:
    """
    Estimate cursor position by finding the cell that just changed to a
    full-block (█) or cursor-like character, or the last modified cell.
    """
    last_changed: tuple[int, int] | None = None

    for r, row in enumerate(curr):
        prev_row = prev[r] if prev and r < len(prev) else []
        for c, cell in enumerate(row):
            prev_cell = prev_row[c] if c < len(prev_row) else None
            if prev_cell != cell:
                last_changed = (r, c)
                # VGA block cursor is typically char 0x00, 0xDB, or a reversed cell
                if cell.char in ("\u2588", "\u2584", " ") and cell.bg != (0, 0, 0):
                    return (r, c)

    return last_changed


# ── Terminal session ──────────────────────────────────────────────────────────

class TerminalSession:
    """One connected xterm.js WebSocket client."""

    def __init__(self, ws: Any) -> None:
        self.ws = ws
        self.connected = True

    async def send(self, data: bytes) -> None:
        if not self.connected:
            return
        try:
            await self.ws.send_bytes(data)
        except Exception:
            self.connected = False

    async def send_json(self, obj: dict) -> None:
        if not self.connected:
            return
        try:
            await self.ws.send_text(json.dumps(obj))
        except Exception:
            self.connected = False


# ── Terminal bridge ───────────────────────────────────────────────────────────

class TerminalBridge:
    """
    Terminal bridge for one node.

    Lifecycle:
      - Created by TerminalBridgeManager when a client connects
      - Kept alive while at least one client is connected
      - Destroyed when idle for > idle_timeout seconds
    """

    def __init__(
        self,
        node_id: str,
        frame_fn: Callable[[], Awaitable[bytes | None]],
        hid_host: str,
        hid_port: int,
        fps: float = 25.0,
        idle_timeout: float = 300.0,
    ) -> None:
        self.node_id = node_id
        self._frame_fn = frame_fn
        self._hid_host = hid_host
        self._hid_port = hid_port
        self._fps = fps
        self._idle_timeout = idle_timeout

        self._ocr = TextCapture()
        self._sessions: list[TerminalSession] = []
        self._prev_grid: list[list[CellState]] | None = None
        self._prev_size: tuple[int, int] = (0, 0)   # (cols, rows)
        self._cursor: tuple[int, int] | None = None
        self._last_activity = time.monotonic()
        self._task: asyncio.Task | None = None
        self._running = False

    # ── Lifecycle ─────────────────────────────────────────────────────────────

    def start(self) -> None:
        if self._running:
            return
        self._running = True
        self._task = asyncio.create_task(
            self._capture_loop(), name=f"terminal-bridge-{self.node_id}"
        )
        log.info("Terminal bridge started for %s", self.node_id)

    def stop(self) -> None:
        self._running = False
        if self._task and not self._task.done():
            self._task.cancel()

    @property
    def idle(self) -> bool:
        return (not self._sessions and
                time.monotonic() - self._last_activity > self._idle_timeout)

    # ── Client sessions ───────────────────────────────────────────────────────

    async def add_session(self, ws: Any) -> TerminalSession:
        session = TerminalSession(ws)
        self._sessions.append(session)
        self._last_activity = time.monotonic()
        # Send full repaint to new client
        if self._prev_grid:
            cols, rows = self._prev_size
            await session.send_json({"type": "resize", "cols": cols, "rows": rows})
            await session.send(full_repaint(self._prev_grid))
            if self._cursor:
                await session.send(
                    f"\x1b[{self._cursor[0]+1};{self._cursor[1]+1}H\x1b[?25h".encode()
                )
        return session

    def remove_session(self, session: TerminalSession) -> None:
        self._sessions = [s for s in self._sessions if s is not session]

    # ── HID input ─────────────────────────────────────────────────────────────

    def send_hid(self, ptype: int, payload: bytes) -> None:
        """Send a raw HID packet to the node over UDP."""
        import socket as _socket
        sock = _socket.socket(_socket.AF_INET, _socket.SOCK_DGRAM)
        try:
            sock.sendto(bytes([ptype]) + payload, (self._hid_host, self._hid_port))
        except OSError:
            pass
        finally:
            sock.close()

    # ── Capture loop ──────────────────────────────────────────────────────────

    async def _capture_loop(self) -> None:
        interval = 1.0 / self._fps
        consecutive_failures = 0

        while self._running:
            t0 = time.monotonic()

            if not self._sessions:
                await asyncio.sleep(interval)
                continue

            try:
                jpeg = await asyncio.wait_for(self._frame_fn(), timeout=3.0)
            except (asyncio.TimeoutError, Exception) as e:
                consecutive_failures += 1
                if consecutive_failures == 3:
                    log.warning("Terminal bridge %s: frame source failing: %s",
                                self.node_id, e)
                await asyncio.sleep(interval)
                continue

            if jpeg is None:
                await asyncio.sleep(interval)
                continue

            consecutive_failures = 0

            try:
                await asyncio.get_event_loop().run_in_executor(
                    None, self._process_frame, jpeg
                )
            except Exception as e:
                log.debug("Terminal bridge %s: OCR error: %s", self.node_id, e)

            elapsed = time.monotonic() - t0
            wait = max(0.0, interval - elapsed)
            await asyncio.sleep(wait)

    def _process_frame(self, jpeg: bytes) -> None:
        """OCR the frame and broadcast ANSI diff to all sessions (sync, runs in executor)."""
        if not _DEPS_AVAILABLE:
            return

        img = _PILImage.open(io.BytesIO(jpeg)).convert("RGB")
        frame_rgb = np.array(img, dtype=np.uint8)

        result = self._ocr.recognise_frame(img)
        if not result.lines:
            return

        grid = _build_cell_grid(frame_rgb, result, self._ocr._fonts)

        new_size = (result.grid_width, result.grid_height)
        size_changed = new_size != self._prev_size

        if size_changed or self._prev_grid is None:
            data = full_repaint(grid)
            resize_msg = json.dumps({"type": "resize",
                                     "cols": result.grid_width,
                                     "rows": result.grid_height}).encode()
        else:
            data = diff_repaint(self._prev_grid, grid)
            resize_msg = None

        cursor = _estimate_cursor(self._prev_grid, grid)
        if cursor:
            r, c = cursor
            cursor_seq = f"\x1b[{r+1};{c+1}H\x1b[?25h".encode()
        else:
            cursor_seq = b"\x1b[?25h"

        self._prev_grid = grid
        self._prev_size = new_size
        self._cursor = cursor

        # Schedule broadcast back on the event loop
        loop = asyncio.get_event_loop()
        loop.call_soon_threadsafe(
            lambda: asyncio.ensure_future(
                self._broadcast(resize_msg, data, cursor_seq)
            )
        )

    async def _broadcast(
        self,
        resize_msg: bytes | None,
        ansi: bytes,
        cursor_seq: bytes,
    ) -> None:
        dead: list[TerminalSession] = []
        for session in list(self._sessions):
            try:
                if resize_msg:
                    await session.send(resize_msg)
                if ansi:
                    await session.send(ansi + cursor_seq)
                elif cursor_seq:
                    await session.send(cursor_seq)
            except Exception:
                dead.append(session)
        for s in dead:
            self.remove_session(s)


# ── Manager ───────────────────────────────────────────────────────────────────

class TerminalBridgeManager:
    """Manages terminal bridges across all nodes."""

    def __init__(self, state: Any, streams: Any = None) -> None:
        self._state = state
        self._streams = streams
        self._bridges: dict[str, TerminalBridge] = {}
        self._reap_task: asyncio.Task | None = None

    def start(self) -> None:
        self._reap_task = asyncio.create_task(
            self._reap_loop(), name="terminal-bridge-reaper"
        )

    def stop(self) -> None:
        for bridge in self._bridges.values():
            bridge.stop()
        if self._reap_task:
            self._reap_task.cancel()

    def get_or_create(self, node_id: str) -> TerminalBridge | None:
        """Get existing bridge or create a new one for the node."""
        if node_id in self._bridges:
            return self._bridges[node_id]

        node = self._state.nodes.get(node_id)
        if not node:
            return None

        frame_fn = self._make_frame_fn(node_id, node)
        bridge = TerminalBridge(
            node_id=node_id,
            frame_fn=frame_fn,
            hid_host=node.host,
            hid_port=node.port,
        )
        bridge.start()
        self._bridges[node_id] = bridge
        return bridge

    def _make_frame_fn(self, node_id: str, node: Any) -> Callable[[], Awaitable[bytes | None]]:
        """Build a frame source for the node, trying the best available method."""
        import httpx

        async def _grab() -> bytes | None:
            # 1. Node HTTP snapshot (fastest for soft nodes: QMP screendump)
            if node.api_port:
                url = f"http://127.0.0.1:{node.api_port}/display/snapshot"
                try:
                    async with httpx.AsyncClient(timeout=2.0) as client:
                        r = await client.get(url)
                        if r.status_code == 200:
                            return r.content
                except Exception:
                    pass

            # 2. Controller stream capture snapshot
            if self._streams:
                try:
                    return await self._streams.get_snapshot(node_id)
                except Exception:
                    pass

            return None

        return _grab

    async def _reap_loop(self) -> None:
        """Clean up idle bridges."""
        while True:
            await asyncio.sleep(60)
            for node_id in list(self._bridges):
                bridge = self._bridges[node_id]
                if bridge.idle:
                    log.info("Reaping idle terminal bridge for %s", node_id)
                    bridge.stop()
                    del self._bridges[node_id]
