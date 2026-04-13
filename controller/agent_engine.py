# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
AI Agent Engine — structured tool interface for AI-driven machine control.

Implements the `ozma_control` tool that AI agents (Claude, GPT, Ollama, etc.)
use to interact with any machine on the ozma mesh.  Compatible with:
  - Anthropic's computer use tool schema (computer_20251124)
  - OpenAI's CUA action schema
  - MCP (Model Context Protocol) tool format

The agent engine combines:
  1. Screen understanding (5-level: bitmap → tesseract → elements → AI → vectors)
  2. Set-of-Marks grounding (numbered overlays on detected elements)
  3. Input injection (keyboard + mouse via UDP HID to nodes)
  4. Action verification (before/after screenshot diff)
  5. Condition-based waits (wait for text, element, screen change)

This is the single entry point for all AI agent interactions.  Every action
goes through here — the engine handles node resolution, screen capture,
input routing, and verification.
"""

from __future__ import annotations

import asyncio
import base64
import io
import logging
import secrets
import socket
import struct
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.agent_engine")

# ── Approval modes ────────────────────────────────────────────────────────────

APPROVAL_AUTO = "auto"       # Execute immediately (current behaviour)
APPROVAL_NOTIFY = "notify"   # Execute immediately, fire notification
APPROVAL_APPROVE = "approve" # Queue for human approval before executing

_READ_ONLY_ACTIONS = frozenset({
    "screenshot", "read_screen", "find_elements", "assert_text",
    "assert_element", "wait_for_text", "wait_for_element", "get_cursor_position",
})

_DEFAULT_APPROVAL: dict[str, str] = {}  # populated in AgentEngine.__init__

# ── Context sources ───────────────────────────────────────────────────────────

CONTEXT_SOURCES = frozenset({
    "slack", "microsoft_graph", "google_workspace"
})


@dataclass
class PendingAction:
    """An action awaiting human approval."""
    action_id: str
    action: str
    node_id: str
    kwargs: dict
    created_at: float
    event: asyncio.Event = field(default_factory=asyncio.Event)
    approved: bool = False

    def to_dict(self) -> dict:
        return {
            "action_id": self.action_id,
            "action": self.action,
            "node_id": self.node_id,
            "kwargs": {k: v for k, v in self.kwargs.items() if k != "verify"},
            "created_at": self.created_at,
        }


# ── HID keycodes ──────────────────────────────────────────────────────────────

_NAMED_KEYS: dict[str, int] = {
    "enter": 0x28, "return": 0x28, "esc": 0x29, "escape": 0x29,
    "backspace": 0x2A, "tab": 0x2B, "space": 0x2C, "delete": 0x4C,
    "up": 0x52, "down": 0x51, "left": 0x50, "right": 0x4F,
    "home": 0x4A, "end": 0x4D, "pageup": 0x4B, "pagedown": 0x4E,
    "f1": 0x3A, "f2": 0x3B, "f3": 0x3C, "f4": 0x3D, "f5": 0x3E,
    "f6": 0x3F, "f7": 0x40, "f8": 0x41, "f9": 0x42, "f10": 0x43,
    "f11": 0x44, "f12": 0x45, "insert": 0x49, "printscreen": 0x46,
    "scrolllock": 0x47, "pause": 0x48, "capslock": 0x39,
    "numlock": 0x53, "menu": 0x65,
}

_MODIFIER_MAP: dict[str, int] = {
    "ctrl": 0x01, "control": 0x01, "shift": 0x02, "alt": 0x04,
    "meta": 0x08, "gui": 0x08, "win": 0x08, "super": 0x08,
    "command": 0x08, "cmd": 0x08,
}

_CHAR_TO_HID: dict[str, tuple[int, int]] = {}
for _i, _c in enumerate("abcdefghijklmnopqrstuvwxyz"):
    _CHAR_TO_HID[_c] = (4 + _i, 0)
for _i, _c in enumerate("1234567890"):
    _CHAR_TO_HID[_c] = (30 + _i, 0)
_CHAR_TO_HID.update({
    " ": (0x2C, 0), "-": (0x2D, 0), "=": (0x2E, 0), "[": (0x2F, 0),
    "]": (0x30, 0), "\\": (0x31, 0), ";": (0x33, 0), "'": (0x34, 0),
    "`": (0x35, 0), ",": (0x36, 0), ".": (0x37, 0), "/": (0x38, 0),
    "\n": (0x28, 0), "\t": (0x2B, 0),
})
# Shifted variants
_SHIFT_CHARS = dict(zip('!@#$%^&*()_+{}|:"~<>?',
                        [30,31,32,33,34,35,36,37,38,39,0x2D,0x2E,0x2F,0x30,0x31,0x33,0x34,0x35,0x36,0x37,0x38]))


@dataclass
class ActionResult:
    """Result of an agent action."""
    success: bool = True
    action: str = ""
    node_id: str = ""
    timestamp: float = 0.0
    error: str | None = None
    # Screen state after action
    screenshot_base64: str = ""
    screen_text: str = ""
    elements: list[dict] = field(default_factory=list)
    som_elements: dict[int, dict] = field(default_factory=dict)
    # Verification
    screen_changed: bool | None = None
    diff_regions: list[dict] = field(default_factory=list)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "success": self.success,
            "action": self.action,
            "node_id": self.node_id,
            "timestamp": self.timestamp,
        }
        if self.error:
            d["error"] = self.error
        if self.screenshot_base64:
            d["screenshot_base64"] = self.screenshot_base64
        if self.screen_text:
            d["screen_text"] = self.screen_text
        if self.elements:
            d["elements"] = self.elements
        if self.som_elements:
            d["som_elements"] = self.som_elements
        if self.screen_changed is not None:
            d["verification"] = {
                "screen_changed": self.screen_changed,
                "diff_regions": self.diff_regions,
            }
        return d


class AgentEngine:
    """
    AI agent control engine.

    Single entry point for all AI agent interactions with ozma nodes.
    Handles screen capture, element detection, input injection, and
    action verification.
    """

    def __init__(self, state: Any, screen_reader: Any = None,
                 text_capture: Any = None,
                 evdev_kbd_path: str = "", evdev_mouse_path: str = "",
                 notifier: Any = None, event_queue: asyncio.Queue | None = None,
                 context_sources: dict[str, Any] | None = None) -> None:
        self._state = state
        self._screen_reader = screen_reader
        self._text_capture = text_capture
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        # Cache: node_id → last screenshot (PIL Image)
        self._last_frames: dict[str, Any] = {}
        # SoM element registry: node_id → {element_id: element_dict}
        self._som_registry: dict[str, dict[int, dict]] = {}
        # Evdev input (for QEMU input-linux — bypasses QMP entirely)
        self._evdev_kbd_path = evdev_kbd_path
        self._evdev_mouse_path = evdev_mouse_path
        # Approval system
        self._notifier = notifier
        self._event_queue = event_queue
        self._approval_config: dict[str, str] = {}  # action → mode
        self._pending: dict[str, PendingAction] = {}
        self._approval_timeout = 120.0  # seconds
        # Context sources for platform integrations
        self._context_sources = context_sources or {}

    # ── Approval management ───────────────────────────────────────────

    def get_approval_config(self) -> dict[str, str]:
        """Return current per-action approval modes."""
        return dict(self._approval_config)

    def set_approval_config(self, config: dict[str, str]) -> None:
        """Update per-action approval modes."""
        for action, mode in config.items():
            if mode in (APPROVAL_AUTO, APPROVAL_NOTIFY, APPROVAL_APPROVE):
                self._approval_config[action] = mode

    def _get_approval_mode(self, action: str, node_id: str = "") -> str:
        """Get the approval mode for an action, considering node machine_class."""
        # Explicit per-action config always wins
        if action in self._approval_config:
            return self._approval_config[action]
        # Servers and kiosks default to auto for everything — they're unattended
        node = self._state.nodes.get(node_id) if node_id else None
        if node and getattr(node, "machine_class", "workstation") in ("server", "kiosk"):
            return APPROVAL_AUTO
        # Workstations: read-only = auto, mutating = notify
        return APPROVAL_AUTO if action in _READ_ONLY_ACTIONS else APPROVAL_NOTIFY

    def list_pending(self) -> list[dict]:
        """List actions awaiting approval."""
        return [p.to_dict() for p in self._pending.values()]

    def approve_action(self, action_id: str) -> bool:
        """Approve a pending action."""
        pending = self._pending.get(action_id)
        if not pending:
            return False
        pending.approved = True
        pending.event.set()
        return True

    def reject_action(self, action_id: str) -> bool:
        """Reject a pending action."""
        pending = self._pending.get(action_id)
        if not pending:
            return False
        pending.approved = False
        pending.event.set()
        return True

    async def _fire_event(self, event: dict) -> None:
        """Fire an event to the WebSocket broadcast queue."""
        if self._event_queue:
            await self._event_queue.put(event)

    async def _notify(self, message: str, **extra: Any) -> None:
        """Send a notification if notifier is available."""
        if self._notifier:
            try:
                await self._notifier.send(message, **extra)
            except Exception:
                pass

    # ── Main dispatch ──────────────────────────────────────────────────

    async def execute(self, action: str, node_id: str = "", **kwargs) -> ActionResult:
        """
        Execute an agent action.

        This is the single entry point that maps to the `ozma_control` MCP tool.
        Respects the approval mode configured for each action type.
        """
        result = ActionResult(action=action, timestamp=time.time())

        # Resolve node
        node = self._state.nodes.get(node_id) if node_id else self._state.get_active_node()
        if not node:
            result.success = False
            result.error = "No node found"
            return result
        resolved_node_id = node.id if hasattr(node, 'id') else node_id
        result.node_id = resolved_node_id

        # Check approval mode (considers node machine_class)
        mode = self._get_approval_mode(action, resolved_node_id)

        if mode == APPROVAL_APPROVE:
            action_id = secrets.token_urlsafe(12)
            pending = PendingAction(
                action_id=action_id, action=action, node_id=resolved_node_id,
                kwargs=kwargs, created_at=time.time(),
            )
            self._pending[action_id] = pending
            await self._fire_event({
                "type": "agent.approval_required",
                "action_id": action_id, "action": action,
                "node_id": resolved_node_id,
                "kwargs": {k: v for k, v in kwargs.items() if k != "verify"},
            })
            await self._notify(f"Agent action requires approval: {action} on {resolved_node_id}")
            try:
                await asyncio.wait_for(pending.event.wait(), self._approval_timeout)
            except asyncio.TimeoutError:
                pass
            finally:
                self._pending.pop(action_id, None)
            if not pending.approved:
                result.success = False
                result.error = "Action rejected or timed out"
                return result

        verify = kwargs.get("verify", True)
        before_frame = None
        if verify and action in ("click", "double_click", "right_click", "type",
                                  "key", "hotkey", "scroll", "mouse_drag"):
            before_frame = await self._capture_frame(node)

        try:
            match action:
                case "screenshot":
                    await self._do_screenshot(result, node, **kwargs)
                case "read_screen":
                    await self._do_read_screen(result, node, **kwargs)
                case "click":
                    await self._do_click(result, node, **kwargs)
                case "double_click":
                    kwargs["count"] = 2
                    await self._do_click(result, node, **kwargs)
                case "right_click":
                    kwargs["button"] = "right"
                    await self._do_click(result, node, **kwargs)
                case "type":
                    await self._do_type(result, node, **kwargs)
                case "key":
                    await self._do_key(result, node, **kwargs)
                case "hotkey":
                    await self._do_hotkey(result, node, **kwargs)
                case "mouse_move":
                    await self._do_mouse_move(result, node, **kwargs)
                case "mouse_drag":
                    await self._do_mouse_drag(result, node, **kwargs)
                case "scroll":
                    await self._do_scroll(result, node, **kwargs)
                case "wait_for_text":
                    await self._do_wait_for_text(result, node, **kwargs)
                case "wait_for_element":
                    await self._do_wait_for_element(result, node, **kwargs)
                case "find_elements":
                    await self._do_find_elements(result, node, **kwargs)
                case "assert_text":
                    await self._do_assert_text(result, node, **kwargs)
                case "assert_element":
                    await self._do_assert_element(result, node, **kwargs)
                case "get_cursor_position":
                    result.success = True  # Would need mouse tracking
                case "get_context":
                    await self._do_get_context(result, **kwargs)
                case _:
                    result.success = False
                    result.error = f"Unknown action: {action}"
        except Exception as e:
            result.success = False
            result.error = str(e)
            log.error("Agent action %s failed: %s", action, e)

        # Action verification: compare before/after
        if verify and before_frame is not None and result.success:
            after_frame = await self._capture_frame(node)
            if after_frame is not None and before_frame is not None:
                result.screen_changed, result.diff_regions = self._diff_frames(
                    before_frame, after_frame
                )

        # Post-execution notification for "notify" mode
        if mode == APPROVAL_NOTIFY and result.success:
            await self._fire_event({
                "type": "agent.action_executed",
                "action": action, "node_id": resolved_node_id,
                "kwargs": {k: v for k, v in kwargs.items() if k != "verify"},
            })

        return result

    # ── Screen capture ─────────────────────────────────────────────────

    async def _capture_frame(self, node: Any) -> Any:
        """Capture a frame from a node via VNC."""
        if not node.vnc_host or not node.vnc_port:
            return None
        try:
            import asyncvnc
            import numpy as np
            from PIL import Image

            async with asyncvnc.connect(node.vnc_host, node.vnc_port) as client:
                frame = await client.screenshot()
                arr = np.array(frame)

                # If frame is all black, the display might be sleeping.
                # Send a wake nudge (mouse move) and retry.
                if arr[:, :, :3].mean() < 1.0:
                    # Nudge the mouse to wake display
                    self._send_mouse(node.host, node.port, 512, 384, 1024, 768)
                    await asyncio.sleep(1)
                    frame = await client.screenshot()
                    arr = np.array(frame)

                img = Image.fromarray(arr[:, :, :3])
                node_id = node.id if hasattr(node, 'id') else ""
                self._last_frames[node_id] = img
                return img
        except Exception as e:
            log.debug("Frame capture failed for %s: %s", node.vnc_host, e)
            return None

    def _frame_to_base64(self, img: Any, quality: int = 70) -> str:
        """Convert a PIL Image to base64 JPEG."""
        buf = io.BytesIO()
        img.save(buf, format="JPEG", quality=quality)
        return base64.b64encode(buf.getvalue()).decode()

    def _diff_frames(self, before: Any, after: Any) -> tuple[bool, list[dict]]:
        """Compare two frames and return (changed, diff_regions)."""
        import numpy as np
        a = np.array(before)
        b = np.array(after)
        if a.shape != b.shape:
            return True, []

        diff = np.abs(a.astype(int) - b.astype(int)).mean(axis=2)
        changed_mask = diff > 15  # threshold for pixel change
        changed_pct = changed_mask.mean() * 100

        if changed_pct < 0.5:
            return False, []

        # Find changed regions (simple grid-based)
        regions = []
        h, w = changed_mask.shape
        grid = 64
        for gy in range(0, h, grid):
            for gx in range(0, w, grid):
                cell = changed_mask[gy:gy + grid, gx:gx + grid]
                cell_pct = cell.mean() * 100
                if cell_pct > 10:
                    regions.append({
                        "x": int(gx), "y": int(gy),
                        "w": min(grid, w - gx), "h": min(grid, h - gy),
                        "change_pct": round(float(cell_pct), 1),
                    })

        return True, regions

    # ── SoM overlay ────────────────────────────────────────────────────

    def _apply_som(self, img: Any, elements: list[dict], node_id: str) -> Any:
        """Overlay Set-of-Marks numbered boxes on detected elements."""
        from PIL import ImageDraw, ImageFont
        annotated = img.copy()
        draw = ImageDraw.Draw(annotated)

        try:
            font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 14)
        except Exception:
            font = ImageFont.load_default()

        registry: dict[int, dict] = {}
        for idx, el in enumerate(elements, 1):
            x = el.get("x", 0)
            y = el.get("y", 0)
            w = el.get("width", el.get("w", 50))
            h = el.get("height", el.get("h", 20))
            cx = x + w // 2
            cy = y + h // 2

            # Draw bounding box
            color = "#FF4444" if el.get("type") == "button" or el.get("clickable") else "#44AAFF"
            draw.rectangle([x, y, x + w, y + h], outline=color, width=2)

            # Draw number label
            label = str(idx)
            label_bg = [x - 2, y - 18, x + len(label) * 10 + 4, y - 2]
            draw.rectangle(label_bg, fill=color)
            draw.text((x + 1, y - 17), label, fill="white", font=font)

            registry[idx] = {
                "id": idx, "type": el.get("type", el.get("element_type", "unknown")),
                "text": el.get("text", el.get("label", "")),
                "x": x, "y": y, "w": w, "h": h,
                "center": [cx, cy], "clickable": el.get("clickable", False),
            }

        self._som_registry[node_id] = registry
        return annotated

    def _resolve_element(self, node_id: str, element_id: int) -> tuple[int, int] | None:
        """Resolve a SoM element ID to click coordinates."""
        registry = self._som_registry.get(node_id, {})
        el = registry.get(element_id)
        if el:
            return tuple(el["center"])
        return None

    # ── Input injection ────────────────────────────────────────────────

    # HID keycode → Linux evdev keycode mapping
    _HID_TO_EVDEV: dict[int, int] = {
        0x04: 30, 0x05: 48, 0x06: 46, 0x07: 32, 0x08: 18, 0x09: 33, 0x0A: 34, 0x0B: 35,
        0x0C: 23, 0x0D: 36, 0x0E: 37, 0x0F: 38, 0x10: 50, 0x11: 49, 0x12: 24, 0x13: 25,
        0x14: 16, 0x15: 19, 0x16: 31, 0x17: 20, 0x18: 22, 0x19: 47, 0x1A: 17, 0x1B: 45,
        0x1C: 21, 0x1D: 44, 0x1E: 2, 0x1F: 3, 0x20: 4, 0x21: 5, 0x22: 6, 0x23: 7,
        0x24: 8, 0x25: 9, 0x26: 10, 0x27: 11, 0x28: 28, 0x29: 1, 0x2A: 14, 0x2B: 15,
        0x2C: 57, 0x2D: 12, 0x2E: 13, 0x2F: 26, 0x30: 27, 0x31: 43, 0x33: 39, 0x34: 40,
        0x35: 41, 0x36: 51, 0x37: 52, 0x38: 53, 0x39: 58, 0x3A: 59, 0x3B: 60, 0x3C: 61,
        0x3D: 62, 0x3E: 63, 0x3F: 64, 0x40: 65, 0x41: 66, 0x42: 67, 0x43: 68, 0x44: 87,
        0x45: 88, 0x49: 110, 0x4A: 102, 0x4B: 104, 0x4C: 111, 0x4D: 107, 0x4E: 109,
        0x4F: 106, 0x50: 105, 0x51: 108, 0x52: 103,
    }
    _HID_MOD_TO_EVDEV: dict[int, int] = {
        0x01: 29, 0x02: 42, 0x04: 56, 0x08: 125,  # ctrl, shift, alt, meta
        0x10: 97, 0x20: 54, 0x40: 100, 0x80: 126,  # right variants
    }

    def _evdev_write(self, fd: int, etype: int, code: int, value: int) -> None:
        import os
        t = time.time()
        os.write(fd, struct.pack('llHHi', int(t), int((t % 1) * 1e6), etype, code, value))

    def _evdev_syn(self, fd: int) -> None:
        self._evdev_write(fd, 0, 0, 0)

    def _evdev_key(self, fd: int, evcode: int, down: bool) -> None:
        self._evdev_write(fd, 1, evcode, 1 if down else 0)
        self._evdev_syn(fd)

    def _send_mouse(self, host: str, port: int, x: int, y: int,
                     width: int, height: int, buttons: int = 0, scroll: int = 0) -> None:
        """Send mouse input — evdev if available, else UDP HID."""
        if self._evdev_mouse_path:
            import os
            try:
                fd = os.open(self._evdev_mouse_path, os.O_WRONLY)
                ax = int(x * 32767 / max(width, 1))
                ay = int(y * 32767 / max(height, 1))
                self._evdev_write(fd, 3, 0, ax)   # ABS_X
                self._evdev_write(fd, 3, 1, ay)   # ABS_Y
                self._evdev_syn(fd)
                if buttons:
                    self._evdev_write(fd, 1, 0x110, 1 if (buttons & 1) else 0)  # BTN_LEFT
                    self._evdev_syn(fd)
                if scroll:
                    val = scroll if scroll < 128 else scroll - 256
                    self._evdev_write(fd, 2, 8, val)  # REL_WHEEL
                    self._evdev_syn(fd)
                os.close(fd)
            except Exception as e:
                log.debug("evdev mouse failed: %s", e)
            return

        ax = int(x * 32767 / max(width, 1))
        ay = int(y * 32767 / max(height, 1))
        packet = bytes([0x02, buttons, ax & 0xFF, (ax >> 8) & 0xFF,
                        ay & 0xFF, (ay >> 8) & 0xFF, scroll & 0xFF])
        self._sock.sendto(packet, (host, port))

    def _send_key(self, host: str, port: int, keycode: int, modifier: int = 0) -> None:
        """Send keyboard input — evdev if available, else UDP HID."""
        if self._evdev_kbd_path:
            import os
            try:
                fd = os.open(self._evdev_kbd_path, os.O_WRONLY)
                # Modifiers
                for bit, evcode in self._HID_MOD_TO_EVDEV.items():
                    if modifier & bit:
                        self._evdev_key(fd, evcode, True)
                # Key
                evcode = self._HID_TO_EVDEV.get(keycode)
                if evcode:
                    self._evdev_key(fd, evcode, True)
                    time.sleep(0.02)
                    self._evdev_key(fd, evcode, False)
                # Release modifiers
                for bit, evcode in self._HID_MOD_TO_EVDEV.items():
                    if modifier & bit:
                        self._evdev_key(fd, evcode, False)
                os.close(fd)
            except Exception as e:
                log.debug("evdev key failed: %s", e)
            return

        press = bytes([0x01, modifier, 0, keycode, 0, 0, 0, 0, 0])
        release = bytes([0x01, 0, 0, 0, 0, 0, 0, 0, 0])
        self._sock.sendto(press, (host, port))
        self._sock.sendto(release, (host, port))

    # ── Action implementations ─────────────────────────────────────────

    async def _do_screenshot(self, result: ActionResult, node: Any, **kw) -> None:
        """Capture and return a screenshot."""
        img = await self._capture_frame(node)
        if img is None:
            result.success = False
            result.error = "Failed to capture screenshot"
            return

        som = kw.get("som", False)
        if som and self._screen_reader:
            screen = await self._screen_reader.read_screen(img)
            elements = [e.to_dict() for e in screen.elements]
            node_id = node.id if hasattr(node, 'id') else ""
            img = self._apply_som(img, elements, node_id)
            result.som_elements = self._som_registry.get(node_id, {})

        result.screenshot_base64 = self._frame_to_base64(img)
        result.success = True

    async def _query_agent_hints(self, node: Any, level: int = 2) -> dict | None:
        """
        Query the agent running inside the target machine for UI hints.

        If the agent is running, this provides window titles, focused control,
        and accessibility tree data — far more reliable than OCR.

        Returns the hints dict, or None if agent is unavailable.
        """
        api_port = getattr(node, 'api_port', None) or 7390
        url = f"http://{node.host}:{api_port}/ui/hints?level={level}"
        try:
            import aiohttp
            async with aiohttp.ClientSession() as session:
                async with session.get(url, timeout=aiohttp.ClientTimeout(total=2)) as resp:
                    if resp.status == 200:
                        return await resp.json()
        except Exception:
            pass
        return None

    async def _do_read_screen(self, result: ActionResult, node: Any, **kw) -> None:
        """
        Read and understand the screen.

        Strategy:
        1. If the node has an agent, query /ui/hints for inside-out UI state
        2. Always capture a screenshot (for SoM overlay + visual verification)
        3. Merge agent hints with OCR results (agent wins for text, OCR for layout)
        4. Fall back to pure OCR if agent is unavailable
        """
        # Step 1: Try agent hints (fast, authoritative)
        hint_level = 3 if kw.get("som") else 2
        agent_hints = await self._query_agent_hints(node, level=hint_level)

        # Step 2: Capture screenshot
        img = await self._capture_frame(node)
        if img is None and not agent_hints:
            result.success = False
            result.error = "Failed to capture screenshot and no agent available"
            return

        # Step 3: If we have agent hints, use them as primary data
        if agent_hints:
            # Windows become text
            windows = agent_hints.get("windows", [])
            focused = agent_hints.get("focused_window")
            controls = agent_hints.get("controls", [])
            focused_ctrl = agent_hints.get("focused_control")

            # Build text from agent data
            text_parts = []
            if focused:
                text_parts.append(focused.get("title", ""))
            if focused_ctrl:
                text_parts.append(f"{focused_ctrl.get('type', '')}: {focused_ctrl.get('name', '')}")
                if focused_ctrl.get("value"):
                    text_parts.append(focused_ctrl["value"])
            for ctrl in controls:
                if ctrl.get("name"):
                    text_parts.append(ctrl["name"])
                if ctrl.get("value"):
                    text_parts.append(ctrl["value"])
                for child in ctrl.get("children", []):
                    if child.get("name"):
                        text_parts.append(child["name"])

            result.screen_text = agent_hints.get("screen_text") or " ".join(text_parts)

            # Convert controls to elements
            for ctrl in controls:
                result.elements.append({
                    "type": ctrl.get("type", "unknown"),
                    "text": ctrl.get("name", ""),
                    "x": ctrl.get("x", 0), "y": ctrl.get("y", 0),
                    "width": ctrl.get("width", 0), "height": ctrl.get("height", 0),
                    "center": ctrl.get("center", [0, 0]),
                    "clickable": ctrl.get("clickable", False),
                    "value": ctrl.get("value", ""),
                    "automation_id": ctrl.get("automation_id", ""),
                    "source": "agent",
                })
                for child in ctrl.get("children", []):
                    result.elements.append({
                        "type": child.get("type", "unknown"),
                        "text": child.get("name", ""),
                        "x": child.get("x", 0), "y": child.get("y", 0),
                        "width": child.get("width", 0), "height": child.get("height", 0),
                        "center": child.get("center", [0, 0]),
                        "clickable": child.get("clickable", False),
                        "source": "agent",
                    })

            # Add window info
            for w in windows:
                if not w.get("minimised"):
                    result.elements.append({
                        "type": "window",
                        "text": w.get("title", ""),
                        "x": w.get("x", 0), "y": w.get("y", 0),
                        "width": w.get("width", 0), "height": w.get("height", 0),
                        "center": [w.get("x", 0) + w.get("width", 0) // 2,
                                   w.get("y", 0) + w.get("height", 0) // 2],
                        "clickable": True,
                        "focused": w.get("focused", False),
                        "process": w.get("process", ""),
                        "source": "agent",
                    })

        # Step 4: Also do OCR if no agent or for additional context
        if img and self._screen_reader:
            level = kw.get("level", "auto")
            # Skip heavy OCR if agent provided good data
            if agent_hints and len(result.elements) > 3:
                level = "tesseract"  # just basic OCR for text, skip vision models
            use_ai = level == "ai_vision"
            screen = await self._screen_reader.read_screen(img, use_ai=use_ai, level=level)

            # Merge: if agent didn't provide text, use OCR text
            if not result.screen_text:
                result.screen_text = screen.raw_text

            # Add OCR-only elements (not already covered by agent)
            if not agent_hints:
                result.elements = [e.to_dict() for e in screen.elements]

        som = kw.get("som", False)
        if som:
            node_id = node.id if hasattr(node, 'id') else ""
            annotated = self._apply_som(img, result.elements, node_id)
            result.screenshot_base64 = self._frame_to_base64(annotated)
            result.som_elements = self._som_registry.get(node_id, {})
        else:
            result.screenshot_base64 = self._frame_to_base64(img)

        result.success = True

    async def _do_click(self, result: ActionResult, node: Any, **kw) -> None:
        """Click at coordinates or element."""
        x, y = kw.get("x"), kw.get("y")
        element_id = kw.get("element_id")
        node_id = node.id if hasattr(node, 'id') else ""

        # Resolve element ID to coordinates
        if element_id is not None and (x is None or y is None):
            coords = self._resolve_element(node_id, element_id)
            if coords:
                x, y = coords
            else:
                result.success = False
                result.error = f"Element [{element_id}] not found. Take a screenshot with som=true first."
                return

        if x is None or y is None:
            result.success = False
            result.error = "Coordinates (x, y) or element_id required"
            return

        # Get screen dimensions from last frame
        last_img = self._last_frames.get(node_id)
        width = last_img.width if last_img else 1024
        height = last_img.height if last_img else 768

        button_name = kw.get("button", "left")
        btn_byte = {"left": 1, "right": 2, "middle": 4}.get(button_name, 1)
        count = kw.get("count", 1)

        # Apply modifiers
        modifiers = kw.get("modifiers", [])
        mod_byte = 0
        for m in modifiers:
            mod_byte |= _MODIFIER_MAP.get(m.lower(), 0)
        if mod_byte:
            self._send_key(node.host, node.port, 0, mod_byte)
            await asyncio.sleep(0.02)

        for _ in range(count):
            # Move to position
            self._send_mouse(node.host, node.port, x, y, width, height)
            await asyncio.sleep(0.02)
            # Press
            self._send_mouse(node.host, node.port, x, y, width, height, buttons=btn_byte)
            await asyncio.sleep(0.03)
            # Release
            self._send_mouse(node.host, node.port, x, y, width, height, buttons=0)
            await asyncio.sleep(0.05)

        # Release modifiers
        if mod_byte:
            self._send_key(node.host, node.port, 0, 0)

        result.success = True

    async def _do_type(self, result: ActionResult, node: Any, **kw) -> None:
        """Type text."""
        text = kw.get("text", "")
        if not text:
            result.success = False
            result.error = "No text provided"
            return

        for ch in text:
            lch = ch.lower()
            if lch in _CHAR_TO_HID:
                kc, mod = _CHAR_TO_HID[lch]
                if ch.isupper() and ch.isalpha():
                    mod |= 0x02
                self._send_key(node.host, node.port, kc, mod)
            elif ch in _SHIFT_CHARS:
                self._send_key(node.host, node.port, _SHIFT_CHARS[ch], 0x02)
            await asyncio.sleep(0.02)

        result.success = True

    async def _do_key(self, result: ActionResult, node: Any, **kw) -> None:
        """Press a single key."""
        key_name = kw.get("key", "").lower()
        keycode = _NAMED_KEYS.get(key_name)

        modifiers = kw.get("modifiers", [])
        mod_byte = 0
        for m in modifiers:
            mod_byte |= _MODIFIER_MAP.get(m.lower(), 0)

        if keycode is None:
            # Try as a single character
            if len(key_name) == 1 and key_name in _CHAR_TO_HID:
                kc, extra_mod = _CHAR_TO_HID[key_name]
                self._send_key(node.host, node.port, kc, mod_byte | extra_mod)
                result.success = True
                return
            result.success = False
            result.error = f"Unknown key: {key_name}"
            return

        self._send_key(node.host, node.port, keycode, mod_byte)
        result.success = True

    async def _do_hotkey(self, result: ActionResult, node: Any, **kw) -> None:
        """Press a key combination (e.g. ctrl+c)."""
        keys = kw.get("keys", [])
        if not keys:
            result.success = False
            result.error = "No keys provided"
            return

        # Separate modifiers from the final key
        mod_byte = 0
        final_key = None
        for k in keys:
            k_lower = k.lower()
            if k_lower in _MODIFIER_MAP:
                mod_byte |= _MODIFIER_MAP[k_lower]
            else:
                final_key = k_lower

        if final_key:
            keycode = _NAMED_KEYS.get(final_key)
            if keycode is None and len(final_key) == 1 and final_key in _CHAR_TO_HID:
                keycode, extra = _CHAR_TO_HID[final_key]
                mod_byte |= extra
            if keycode:
                self._send_key(node.host, node.port, keycode, mod_byte)
                result.success = True
                return

        result.success = False
        result.error = f"Could not resolve hotkey: {keys}"

    async def _do_mouse_move(self, result: ActionResult, node: Any, **kw) -> None:
        """Move mouse without clicking."""
        x, y = kw.get("x", 0), kw.get("y", 0)
        node_id = node.id if hasattr(node, 'id') else ""
        last_img = self._last_frames.get(node_id)
        width = last_img.width if last_img else 1024
        height = last_img.height if last_img else 768
        self._send_mouse(node.host, node.port, x, y, width, height)
        result.success = True

    async def _do_mouse_drag(self, result: ActionResult, node: Any, **kw) -> None:
        """Drag from (x,y) to (end_x, end_y)."""
        x, y = kw.get("x", 0), kw.get("y", 0)
        ex, ey = kw.get("end_x", x), kw.get("end_y", y)
        node_id = node.id if hasattr(node, 'id') else ""
        last_img = self._last_frames.get(node_id)
        width = last_img.width if last_img else 1024
        height = last_img.height if last_img else 768

        # Move to start, press, move to end, release
        self._send_mouse(node.host, node.port, x, y, width, height)
        await asyncio.sleep(0.03)
        self._send_mouse(node.host, node.port, x, y, width, height, buttons=1)
        await asyncio.sleep(0.05)

        # Interpolate path
        steps = max(10, abs(ex - x) // 10 + abs(ey - y) // 10)
        for i in range(1, steps + 1):
            cx = x + (ex - x) * i // steps
            cy = y + (ey - y) * i // steps
            self._send_mouse(node.host, node.port, cx, cy, width, height, buttons=1)
            await asyncio.sleep(0.01)

        self._send_mouse(node.host, node.port, ex, ey, width, height, buttons=0)
        result.success = True

    async def _do_scroll(self, result: ActionResult, node: Any, **kw) -> None:
        """Scroll at position."""
        x = kw.get("x", 512)
        y = kw.get("y", 384)
        direction = kw.get("direction", "down")
        amount = kw.get("amount", 3)
        node_id = node.id if hasattr(node, 'id') else ""
        last_img = self._last_frames.get(node_id)
        width = last_img.width if last_img else 1024
        height = last_img.height if last_img else 768

        scroll_val = (-amount) & 0xFF if direction == "down" else amount & 0xFF
        for _ in range(abs(amount)):
            self._send_mouse(node.host, node.port, x, y, width, height,
                             scroll=1 if direction == "up" else 0xFF)
            await asyncio.sleep(0.05)
        result.success = True

    async def _do_wait_for_text(self, result: ActionResult, node: Any, **kw) -> None:
        """Wait until text appears on screen."""
        text = kw.get("text", "")
        timeout = kw.get("timeout", 60)
        level = kw.get("level", "auto")

        if not text:
            result.success = False
            result.error = "No text to wait for"
            return

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            img = await self._capture_frame(node)
            if img and self._screen_reader:
                screen = await self._screen_reader.read_screen(img)
                if text.lower() in screen.raw_text.lower():
                    result.success = True
                    result.screen_text = screen.raw_text
                    result.screenshot_base64 = self._frame_to_base64(img)
                    return
            await asyncio.sleep(1)

        result.success = False
        result.error = f"Timeout waiting for text: {text!r}"

    async def _do_wait_for_element(self, result: ActionResult, node: Any, **kw) -> None:
        """Wait until a UI element appears."""
        description = kw.get("description", "")
        element_type = kw.get("element_type", "")
        timeout = kw.get("timeout", 60)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            img = await self._capture_frame(node)
            if img and self._screen_reader:
                screen = await self._screen_reader.read_screen(img)
                for el in screen.elements:
                    if element_type and el.element_type != element_type:
                        continue
                    if description and description.lower() not in el.text.lower():
                        continue
                    result.success = True
                    result.elements = [el.to_dict()]
                    result.screenshot_base64 = self._frame_to_base64(img)
                    return
            await asyncio.sleep(1)

        result.success = False
        result.error = f"Timeout waiting for element: {description or element_type}"

    async def _do_find_elements(self, result: ActionResult, node: Any, **kw) -> None:
        """Find all UI elements on screen."""
        if not self._screen_reader:
            result.success = False
            result.error = "Screen reader not available"
            return

        img = await self._capture_frame(node)
        if not img:
            result.success = False
            result.error = "Failed to capture screenshot"
            return

        screen = await self._screen_reader.read_screen(img)
        result.elements = [e.to_dict() for e in screen.elements]
        result.screen_text = screen.raw_text

        som = kw.get("som", True)
        if som:
            node_id = node.id if hasattr(node, 'id') else ""
            annotated = self._apply_som(img, result.elements, node_id)
            result.screenshot_base64 = self._frame_to_base64(annotated)
            result.som_elements = self._som_registry.get(node_id, {})
        else:
            result.screenshot_base64 = self._frame_to_base64(img)

        result.success = True

    async def _do_assert_text(self, result: ActionResult, node: Any, **kw) -> None:
        """Assert that text is present on screen."""
        text = kw.get("text", "")
        img = await self._capture_frame(node)
        if not img or not self._screen_reader:
            result.success = False
            result.error = "Cannot read screen"
            return

        screen = await self._screen_reader.read_screen(img)
        found = text.lower() in screen.raw_text.lower()
        result.success = found
        result.screen_text = screen.raw_text
        if not found:
            result.error = f"Text not found: {text!r}"
            result.screenshot_base64 = self._frame_to_base64(img)

    async def _do_assert_element(self, result: ActionResult, node: Any, **kw) -> None:
        """Assert that a UI element is present."""
        description = kw.get("description", "")
        element_type = kw.get("element_type", "")
        img = await self._capture_frame(node)
        if not img or not self._screen_reader:
            result.success = False
            result.error = "Cannot read screen"
            return

        screen = await self._screen_reader.read_screen(img)
        for el in screen.elements:
            if element_type and el.element_type != element_type:
                continue
            if description and description.lower() not in el.text.lower():
                continue
            result.success = True
            result.elements = [el.to_dict()]
            return

        result.success = False
        result.error = f"Element not found: {description or element_type}"
        result.screenshot_base64 = self._frame_to_base64(img)

    async def _do_get_context(self, result: ActionResult, **kw) -> None:
        """Get context from platform integrations."""
        source = kw.get("source", "")
        query = kw.get("query", "")
        
        if not source:
            result.success = False
            result.error = "No context source specified"
            return
            
        if source not in self._context_sources:
            result.success = False
            result.error = f"Context source '{source}' not available"
            return
            
        try:
            context_provider = self._context_sources[source]
            context_data = await context_provider.get_context(query)
            result.success = True
            result.screen_text = str(context_data)
        except Exception as e:
            result.success = False
            result.error = f"Failed to get context from {source}: {str(e)}"


# ── MCP tool definition ───────────────────────────────────────────────────────

OZMA_CONTROL_TOOL = {
    "name": "ozma_control",
    "description": (
        "Control a machine connected to the Ozma KVM mesh. "
        "Supports screenshot capture, screen reading (OCR), keyboard/mouse input, "
        "and UI element detection. Works on any machine — no agent or OS required. "
        "Use som=true with screenshot/read_screen to get numbered element overlays, "
        "then click by element_id instead of coordinates for much better accuracy."
    ),
    "input_schema": {
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "screenshot", "read_screen", "click", "double_click",
                    "right_click", "type", "key", "hotkey", "mouse_move",
                    "mouse_drag", "scroll", "wait_for_text", "wait_for_element",
                    "find_elements", "assert_text", "assert_element",
                ],
            },
            "node_id": {"type": "string", "description": "Target node. Omit for active node."},
            "x": {"type": "integer", "description": "X pixel coordinate"},
            "y": {"type": "integer", "description": "Y pixel coordinate"},
            "element_id": {"type": "integer", "description": "Click numbered element from SoM overlay"},
            "text": {"type": "string"},
            "key": {"type": "string", "description": "Key name (enter, tab, f2, etc.)"},
            "keys": {"type": "array", "items": {"type": "string"}, "description": "Hotkey combo"},
            "modifiers": {"type": "array", "items": {"type": "string"}},
            "button": {"type": "string", "enum": ["left", "right", "middle"]},
            "direction": {"type": "string", "enum": ["up", "down"]},
            "amount": {"type": "integer"},
            "end_x": {"type": "integer"}, "end_y": {"type": "integer"},
            "timeout": {"type": "number"},
            "som": {"type": "boolean", "description": "Include Set-of-Marks numbered overlays"},
            "verify": {"type": "boolean", "description": "Verify action with before/after diff"},
            "level": {"type": "string", "enum": ["bitmap", "tesseract", "elements", "ai_vision", "auto"]},
            "description": {"type": "string", "description": "Element description for search"},
        },
        "required": ["action"],
    },
}
