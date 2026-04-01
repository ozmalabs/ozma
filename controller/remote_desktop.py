# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Remote desktop via browser — full KVM access from any web browser.

Combines the existing HDMI capture pipeline (HLS/MJPEG) with WebSocket-
based keyboard/mouse input from the browser.  The result is a complete
remote desktop solution that:

  - Works on any machine (BIOS, locked OS, no-OS — anything with HDMI)
  - Requires no software on the target machine
  - Accessible from any device with a web browser
  - Immune to anti-cheat, DRM, screen capture restrictions

Architecture:
  Browser → WebSocket (keyboard/mouse events)
    → Controller → UDP HID packet → Node → USB gadget → Target machine

  Target machine → HDMI → Capture card → ffmpeg → HLS/MJPEG
    → Controller → HLS.js/img tag → Browser

The web page at /remote/{node_id} provides:
  - Live display via HLS.js (or MJPEG fallback)
  - Keyboard capture (all keys including F-keys, modifiers, system keys)
  - Mouse capture (absolute position, buttons, scroll)
  - Clipboard paste (paste-as-typing)
  - Touchscreen support (tap = click, drag = mouse move)
  - Latency indicator
  - OCR button (read text from screen)
  - Recording toggle
"""

from __future__ import annotations

import asyncio
import json
import logging
import secrets
import socket
import time
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from fastapi import WebSocket as FastAPIWebSocket

log = logging.getLogger("ozma.remote_desktop")


class SessionState(str, Enum):
    PENDING = "pending"
    ACTIVE = "active"
    REJECTED = "rejected"
    TIMED_OUT = "timed_out"
    ENDED = "ended"

# HID usage IDs for common keys (browser KeyboardEvent.code → HID)
_KEY_TO_HID: dict[str, int] = {
    "KeyA": 0x04, "KeyB": 0x05, "KeyC": 0x06, "KeyD": 0x07, "KeyE": 0x08,
    "KeyF": 0x09, "KeyG": 0x0A, "KeyH": 0x0B, "KeyI": 0x0C, "KeyJ": 0x0D,
    "KeyK": 0x0E, "KeyL": 0x0F, "KeyM": 0x10, "KeyN": 0x11, "KeyO": 0x12,
    "KeyP": 0x13, "KeyQ": 0x14, "KeyR": 0x15, "KeyS": 0x16, "KeyT": 0x17,
    "KeyU": 0x18, "KeyV": 0x19, "KeyW": 0x1A, "KeyX": 0x1B, "KeyY": 0x1C,
    "KeyZ": 0x1D, "Digit1": 0x1E, "Digit2": 0x1F, "Digit3": 0x20,
    "Digit4": 0x21, "Digit5": 0x22, "Digit6": 0x23, "Digit7": 0x24,
    "Digit8": 0x25, "Digit9": 0x26, "Digit0": 0x27,
    "Enter": 0x28, "Escape": 0x29, "Backspace": 0x2A, "Tab": 0x2B,
    "Space": 0x2C, "Minus": 0x2D, "Equal": 0x2E,
    "BracketLeft": 0x2F, "BracketRight": 0x30, "Backslash": 0x31,
    "Semicolon": 0x33, "Quote": 0x34, "Backquote": 0x35,
    "Comma": 0x36, "Period": 0x37, "Slash": 0x38,
    "CapsLock": 0x39, "F1": 0x3A, "F2": 0x3B, "F3": 0x3C, "F4": 0x3D,
    "F5": 0x3E, "F6": 0x3F, "F7": 0x40, "F8": 0x41,
    "F9": 0x42, "F10": 0x43, "F11": 0x44, "F12": 0x45,
    "PrintScreen": 0x46, "ScrollLock": 0x47, "Pause": 0x48,
    "Insert": 0x49, "Home": 0x4A, "PageUp": 0x4B,
    "Delete": 0x4C, "End": 0x4D, "PageDown": 0x4E,
    "ArrowRight": 0x4F, "ArrowLeft": 0x50, "ArrowDown": 0x51, "ArrowUp": 0x52,
}

_MOD_BITS = {
    "ControlLeft": 0x01, "ShiftLeft": 0x02, "AltLeft": 0x04, "MetaLeft": 0x08,
    "ControlRight": 0x10, "ShiftRight": 0x20, "AltRight": 0x40, "MetaRight": 0x80,
}


class RemoteDesktopSession:
    """
    A single remote desktop session for one node.

    Receives keyboard/mouse events from a browser WebSocket and
    forwards them as HID packets to the node.
    """

    def __init__(self, node_id: str, host: str, port: int,
                 session_id: str = "") -> None:
        self.session_id = session_id or secrets.token_urlsafe(12)
        self.node_id = node_id
        self._host = host
        self._port = port
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._modifiers = 0
        self._pressed_keys: list[int] = []
        self._mouse_buttons = 0
        # Session lifecycle
        self.state = SessionState.PENDING
        self.created_at = time.time()
        self.last_activity = time.time()
        self.requester: str = ""
        self.privacy_mode: bool = False
        self._approval_event = asyncio.Event()
        self._approved = False

    def approve(self) -> None:
        self._approved = True
        self._approval_event.set()

    def reject(self) -> None:
        self._approved = False
        self._approval_event.set()

    async def wait_for_approval(self, timeout: float = 60.0) -> bool:
        """Wait for human approval. Returns True if approved."""
        try:
            await asyncio.wait_for(self._approval_event.wait(), timeout)
        except asyncio.TimeoutError:
            self.state = SessionState.TIMED_OUT
            return False
        if self._approved:
            self.state = SessionState.ACTIVE
            return True
        self.state = SessionState.REJECTED
        return False

    def to_dict(self) -> dict:
        return {
            "session_id": self.session_id,
            "node_id": self.node_id,
            "state": self.state.value,
            "created_at": self.created_at,
            "last_activity": self.last_activity,
            "requester": self.requester,
            "privacy_mode": self.privacy_mode,
        }

    async def handle_ws(self, ws: FastAPIWebSocket) -> None:
        """Handle a WebSocket connection from the browser."""
        await ws.accept()
        self.state = SessionState.ACTIVE
        self.last_activity = time.time()
        log.info("Remote desktop session started: %s (session %s)",
                 self.node_id, self.session_id)

        try:
            while True:
                data = await ws.receive_text()
                self.last_activity = time.time()
                msg = json.loads(data)
                event_type = msg.get("type", "")

                match event_type:
                    case "keydown":
                        self._on_key_down(msg.get("code", ""))
                    case "keyup":
                        self._on_key_up(msg.get("code", ""))
                    case "mousemove":
                        self._on_mouse_move(msg.get("x", 0), msg.get("y", 0),
                                             msg.get("w", 1920), msg.get("h", 1080))
                    case "mousedown":
                        self._on_mouse_button(msg.get("button", 0), True)
                    case "mouseup":
                        self._on_mouse_button(msg.get("button", 0), False)
                    case "wheel":
                        self._on_scroll(msg.get("deltaY", 0))
                    case "paste":
                        await self._on_paste(msg.get("text", ""))

        except Exception:
            pass
        finally:
            self._sock.close()
            log.info("Remote desktop session ended: %s", self.node_id)

    def _on_key_down(self, code: str) -> None:
        if code in _MOD_BITS:
            self._modifiers |= _MOD_BITS[code]
        elif code in _KEY_TO_HID:
            hid = _KEY_TO_HID[code]
            if hid not in self._pressed_keys:
                self._pressed_keys.append(hid)
                if len(self._pressed_keys) > 6:
                    self._pressed_keys.pop(0)
        self._send_kbd_report()

    def _on_key_up(self, code: str) -> None:
        if code in _MOD_BITS:
            self._modifiers &= ~_MOD_BITS[code]
        elif code in _KEY_TO_HID:
            hid = _KEY_TO_HID[code]
            if hid in self._pressed_keys:
                self._pressed_keys.remove(hid)
        self._send_kbd_report()

    def _on_mouse_move(self, x: float, y: float, w: int, h: int) -> None:
        # Convert from browser coordinates to 0-32767 absolute
        abs_x = max(0, min(32767, int(x * 32767 / max(w, 1))))
        abs_y = max(0, min(32767, int(y * 32767 / max(h, 1))))
        report = bytes([self._mouse_buttons,
                        abs_x & 0xFF, (abs_x >> 8) & 0xFF,
                        abs_y & 0xFF, (abs_y >> 8) & 0xFF, 0])
        self._send(0x02, report)

    def _on_mouse_button(self, button: int, pressed: bool) -> None:
        bit = {0: 0x01, 2: 0x02, 1: 0x04}.get(button, 0)
        if pressed:
            self._mouse_buttons |= bit
        else:
            self._mouse_buttons &= ~bit
        # Send move report with updated buttons (position unchanged)
        report = bytes([self._mouse_buttons, 0, 0, 0, 0, 0])
        self._send(0x02, report)

    def _on_scroll(self, delta_y: float) -> None:
        scroll = max(-127, min(127, int(-delta_y / 120)))
        report = bytes([self._mouse_buttons, 0, 0, 0, 0, scroll & 0xFF])
        self._send(0x02, report)

    async def _on_paste(self, text: str) -> None:
        """Paste text by typing it character by character."""
        from paste_typing import LAYOUTS
        layout = LAYOUTS.get("us", {})
        for char in text[:1000]:
            stroke = layout.get(char)
            if not stroke:
                continue
            self._send(0x01, bytes([stroke.modifier, 0, stroke.key, 0, 0, 0, 0, 0]))
            await asyncio.sleep(0.02)
            self._send(0x01, bytes(8))
            await asyncio.sleep(0.015)

    def _send_kbd_report(self) -> None:
        keys = (self._pressed_keys + [0] * 6)[:6]
        report = bytes([self._modifiers, 0] + keys)
        self._send(0x01, report)

    def _send(self, ptype: int, payload: bytes) -> None:
        try:
            self._sock.sendto(bytes([ptype]) + payload, (self._host, self._port))
        except OSError:
            pass


class RemoteDesktopManager:
    """Manages remote desktop sessions with consent and privacy controls."""

    def __init__(self, state: Any, event_queue: asyncio.Queue | None = None,
                 notifier: Any = None, idle_timeout: float = 1800.0) -> None:
        self._state = state
        self._event_queue = event_queue
        self._notifier = notifier
        self._idle_timeout = idle_timeout  # 30 min default
        self._sessions: dict[str, RemoteDesktopSession] = {}  # session_id → session
        # When True, workstation nodes require consent before remote desktop.
        # Server and kiosk nodes never require consent regardless of this flag.
        self._consent_for_workstations = False
        self._idle_task: asyncio.Task | None = None

    def create_session(self, node_id: str, requester: str = "") -> RemoteDesktopSession | None:
        """Create a new session in PENDING state."""
        node = self._state.nodes.get(node_id)
        if not node:
            return None
        session = RemoteDesktopSession(node_id, node.host, node.port)
        session.requester = requester
        self._sessions[session.session_id] = session
        return session

    def get_session(self, session_id: str) -> RemoteDesktopSession | None:
        return self._sessions.get(session_id)

    def approve_session(self, session_id: str) -> bool:
        session = self._sessions.get(session_id)
        if not session or session.state != SessionState.PENDING:
            return False
        session.approve()
        return True

    def reject_session(self, session_id: str) -> bool:
        session = self._sessions.get(session_id)
        if not session or session.state != SessionState.PENDING:
            return False
        session.reject()
        return True

    def end_session(self, session_id: str) -> bool:
        session = self._sessions.pop(session_id, None)
        if not session:
            return False
        session.state = SessionState.ENDED
        return True

    def list_sessions(self) -> list[dict]:
        return [s.to_dict() for s in self._sessions.values()]

    async def fire_event(self, event: dict) -> None:
        if self._event_queue:
            await self._event_queue.put(event)

    def _needs_consent(self, node_id: str) -> bool:
        """Check if a node requires consent for remote desktop access."""
        node = self._state.nodes.get(node_id)
        if not node:
            return False
        # Servers and kiosks never need consent — they're unattended.
        if node.machine_class in ("server", "kiosk"):
            return False
        # Workstations only need consent if the flag is enabled.
        return self._consent_for_workstations

    async def start_session_with_consent(self, session: RemoteDesktopSession) -> bool:
        """
        Run the consent flow for a session.

        Consent depends on the node's machine_class:
          - server/kiosk: always auto-approve (no one to consent)
          - workstation: requires approval only if _consent_for_workstations is True
        """
        if not self._needs_consent(session.node_id):
            session.state = SessionState.ACTIVE
            await self.fire_event({
                "type": "remote_desktop.active",
                "session_id": session.session_id,
                "node_id": session.node_id,
            })
            return True

        await self.fire_event({
            "type": "remote_desktop.consent_request",
            "session_id": session.session_id,
            "node_id": session.node_id,
            "requester": session.requester,
        })
        if self._notifier:
            try:
                await self._notifier.send(
                    f"Remote desktop access requested for {session.node_id} by {session.requester or 'unknown'}")
            except Exception:
                pass

        approved = await session.wait_for_approval(timeout=60.0)
        if approved:
            await self.fire_event({
                "type": "remote_desktop.active",
                "session_id": session.session_id,
                "node_id": session.node_id,
            })
        return approved

    async def start_idle_monitor(self) -> None:
        """Background task that terminates idle sessions."""
        while True:
            await asyncio.sleep(30)
            now = time.time()
            expired = [
                sid for sid, s in self._sessions.items()
                if s.state == SessionState.ACTIVE
                and (now - s.last_activity) > self._idle_timeout
            ]
            for sid in expired:
                session = self._sessions.get(sid)
                if session:
                    log.info("Idle timeout: ending session %s for %s", sid, session.node_id)
                    session.state = SessionState.TIMED_OUT
                    await self.fire_event({
                        "type": "remote_desktop.ended",
                        "session_id": sid,
                        "node_id": session.node_id,
                        "reason": "idle_timeout",
                    })
                    self._sessions.pop(sid, None)
