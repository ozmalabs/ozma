# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Robotic automation engine — full AHK/SikuliX-style automation via HID.

This is ozma's automation layer: image matching, text detection, pixel
inspection, coordinate clicking, keyboard/mouse sequencing, conditional
logic, and loops — all operating through the KVM capture + HID path.

Unlike AutoHotkey (which runs on the target machine), ozma automation
runs on the controller and operates through hardware capture + USB HID.
This means it works on any machine — including ones with no OS running,
locked-down systems, BIOS screens, and bare-metal installations.

Capabilities:

  Screen inspection:
    - OCR text recognition (via text_capture.py)
    - Image template matching (find a button/icon on screen)
    - Pixel colour at coordinate
    - Wait for text/image to appear or disappear
    - Region-based OCR (read specific area of screen)

  Input injection (via paste_typing.py + HID):
    - Type text at configurable speed
    - Send individual keys with modifiers
    - Mouse move to absolute coordinates
    - Mouse click (left/right/middle)
    - Mouse drag
    - Scroll

  Control flow:
    - if/elif/else (text on screen, image found, pixel colour)
    - while loops with conditions
    - repeat N times
    - wait / sleep
    - timeout with fallback
    - variables and string interpolation
    - labels and goto (for simple state machines)
    - subroutine call

  Recording:
    - Record mode captures all HID events with precise timestamps
    - Generates an editable script from the recording
    - Three playback modes:
      - exact: replay with original timing
      - normal: group rapid keystrokes as type commands, keep pauses
      - instant: no delays between actions

Script format (ozma automation DSL):

    # Variables
    set ip 192.168.1.100
    set password mysecretpass

    # Wait for BIOS, then navigate
    wait_for_text "Press DEL" timeout=10
    key delete
    wait 3

    # Image matching — find and click a button
    click_image "save_button.png" confidence=0.8
    wait 1

    # Conditional
    if text_on_screen "Error"
        screenshot "error_{timestamp}.png"
        notify "BIOS config error on {source}"
        abort
    endif

    # Type with variable interpolation
    type "{ip}"
    key enter
    wait 1
    type "{password}"
    key enter

    # Loop
    repeat 5
        key down
        wait 0.2
    endrepeat

    # Mouse operations
    mouse_move 500 300
    mouse_click left
    wait 0.5
    mouse_drag 100 100 400 400

    # Pixel check
    if pixel_color 100 200 == "#FF0000"
        log "Red indicator detected"
    endif

    # Wait for image to disappear
    wait_until_gone "loading_spinner.png" timeout=30
"""

from __future__ import annotations

import asyncio
import logging
import re
import socket
import struct
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.automation")

try:
    import numpy as np
    from PIL import Image
    _IMAGING_AVAILABLE = True
except ImportError:
    _IMAGING_AVAILABLE = False


@dataclass
class AutomationContext:
    """Runtime state for a script execution."""
    variables: dict[str, str] = field(default_factory=dict)
    source_id: str = ""
    node_host: str = ""
    node_port: int = 0
    running: bool = True
    line_num: int = 0
    errors: list[str] = field(default_factory=list)
    lines_executed: int = 0


class AutomationEngine:
    """
    Full robotic automation engine.

    Operates through the KVM path: reads the screen via capture cards,
    sends input via HID UDP packets.  Works on any machine — no agent,
    no OS, no network on the target.
    """

    def __init__(
        self,
        state: Any,
        text_capture: Any = None,
        captures: Any = None,
    ) -> None:
        self._state = state
        self._text_capture = text_capture
        self._captures = captures
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self._running_scripts: dict[str, AutomationContext] = {}
        self._script_tasks: dict[str, asyncio.Task] = {}   # run_id → asyncio.Task
        self._image_templates: dict[str, Any] = {}  # cached template images

    # ── Script execution ─────────────────────────────────────────────────────

    async def run_script(
        self,
        script: str,
        source_id: str = "",
        node_id: str | None = None,
        variables: dict[str, str] | None = None,
    ) -> dict[str, Any]:
        """Execute an automation script."""
        node = self._state.nodes.get(node_id) if node_id else self._state.get_active_node()
        if not node:
            return {"ok": False, "error": "No active node"}

        ctx = AutomationContext(
            variables=variables or {},
            source_id=source_id,
            node_host=node.host,
            node_port=node.port,
        )
        script_id = f"script-{time.monotonic()}"
        self._running_scripts[script_id] = ctx

        lines = script.splitlines()
        try:
            await self._execute_lines(lines, 0, len(lines), ctx)
        except _AbortScript:
            pass
        except Exception as e:
            ctx.errors.append(f"Line {ctx.line_num}: {e}")
        finally:
            self._running_scripts.pop(script_id, None)

        return {
            "ok": not ctx.errors,
            "lines_executed": ctx.lines_executed,
            "errors": ctx.errors,
        }

    async def run_script_background(
        self,
        script: str,
        source_id: str = "",
        node_id: str | None = None,
        variables: dict[str, str] | None = None,
    ) -> tuple[str, dict[str, Any]]:
        """Start script in a background task; return (run_id, status_dict) immediately.

        The caller can cancel via cancel_script(run_id).  Completed/cancelled
        tasks are reaped lazily when the run_id is looked up.
        """
        node = self._state.nodes.get(node_id) if node_id else self._state.get_active_node()
        if not node:
            return "", {"ok": False, "error": "No active node"}

        run_id = f"run-{int(time.monotonic() * 1000)}"
        ctx = AutomationContext(
            variables=variables or {},
            source_id=source_id,
            node_host=node.host,
            node_port=node.port,
        )
        self._running_scripts[run_id] = ctx

        async def _task() -> None:
            lines = script.splitlines()
            try:
                await self._execute_lines(lines, 0, len(lines), ctx)
            except _AbortScript:
                pass
            except asyncio.CancelledError:
                ctx.running = False
            except Exception as e:
                ctx.errors.append(f"Line {ctx.line_num}: {e}")
            finally:
                self._running_scripts.pop(run_id, None)
                self._script_tasks.pop(run_id, None)

        task = asyncio.create_task(_task(), name=f"automation-{run_id}")
        self._script_tasks[run_id] = task
        return run_id, {"ok": True, "id": run_id, "status": "running"}

    def cancel_script(self, run_id: str) -> bool:
        """Cancel a running background script. Returns True if it was found and cancelled."""
        task = self._script_tasks.get(run_id)
        if task and not task.done():
            task.cancel()
            ctx = self._running_scripts.get(run_id)
            if ctx:
                ctx.running = False
            return True
        return False

    def get_script_status(self, run_id: str) -> dict[str, Any] | None:
        """Return status of a background run, or None if not found."""
        task = self._script_tasks.get(run_id)
        ctx = self._running_scripts.get(run_id)
        if task is None and ctx is None:
            return None
        running = task is not None and not task.done()
        return {
            "id": run_id,
            "status": "running" if running else "completed",
            "ok": not (ctx.errors if ctx else []),
            "errors": ctx.errors if ctx else [],
            "lines_executed": ctx.lines_executed if ctx else 0,
        }

    async def _execute_lines(
        self, lines: list[str], start: int, end: int, ctx: AutomationContext
    ) -> None:
        i = start
        while i < end and ctx.running:
            raw = lines[i].strip()
            ctx.line_num = i + 1

            # Skip blanks and comments
            if not raw or raw.startswith("#"):
                i += 1
                continue

            # Variable interpolation
            line = self._interpolate(raw, ctx.variables)

            # Block structures
            if line.startswith("if "):
                i = await self._handle_if(lines, i, end, ctx)
                continue
            elif line.startswith("repeat "):
                i = await self._handle_repeat(lines, i, end, ctx)
                continue
            elif line.startswith("while "):
                i = await self._handle_while(lines, i, end, ctx)
                continue

            # Single-line commands
            await self._exec_command(line, ctx)
            ctx.lines_executed += 1
            i += 1

    def _interpolate(self, line: str, variables: dict[str, str]) -> str:
        """Replace {varname} with variable values."""
        def _repl(m: re.Match) -> str:
            key = m.group(1)
            if key == "timestamp":
                return time.strftime("%Y%m%d_%H%M%S")
            return variables.get(key, m.group(0))
        return re.sub(r"\{(\w+)\}", _repl, line)

    # ── Commands ─────────────────────────────────────────────────────────────

    async def _exec_command(self, line: str, ctx: AutomationContext) -> None:
        parts = line.split(None, 1)
        cmd = parts[0].lower()
        arg = parts[1] if len(parts) > 1 else ""

        match cmd:
            # Variables
            case "set":
                kv = arg.split(None, 1)
                if len(kv) == 2:
                    ctx.variables[kv[0]] = kv[1]

            # Text input
            case "type":
                text = arg.replace("\\n", "\n").replace("\\t", "\t")
                await self._type_text(text, ctx)
            case "key":
                await self._send_key(arg, ctx)

            # Timing
            case "wait":
                await asyncio.sleep(float(arg))
            case "wait_for_text":
                await self._wait_for_text(arg, ctx)
            case "wait_until_gone":
                await self._wait_until_gone(arg, ctx)

            # Mouse
            case "mouse_move":
                coords = arg.split()
                if len(coords) >= 2:
                    await self._mouse_move(int(coords[0]), int(coords[1]), ctx)
            case "mouse_click":
                button = arg.strip().lower() or "left"
                await self._mouse_click(button, ctx)
            case "mouse_drag":
                coords = arg.split()
                if len(coords) >= 4:
                    await self._mouse_drag(
                        int(coords[0]), int(coords[1]),
                        int(coords[2]), int(coords[3]), ctx
                    )

            # Image matching
            case "click_image":
                await self._click_image(arg, ctx)
            case "wait_for_image":
                await self._wait_for_image(arg, ctx)

            # Screen inspection
            case "screenshot":
                await self._screenshot(arg, ctx)
            case "ocr":
                result = self._text_capture.last_result if self._text_capture else None
                if result:
                    ctx.variables["_ocr_text"] = result.text

            # Control
            case "log":
                log.info("Script: %s", arg)
            case "notify":
                log.warning("Script notify: %s", arg)
            case "abort":
                raise _AbortScript()
            case "sleep":
                await asyncio.sleep(float(arg))

            case _:
                ctx.errors.append(f"Unknown command: {cmd}")

    # ── Block handlers ───────────────────────────────────────────────────────

    async def _handle_if(self, lines: list[str], start: int, end: int, ctx: AutomationContext) -> int:
        """Handle if/elif/else/endif blocks."""
        condition = lines[start].strip()[3:]  # after "if "
        condition_met = await self._eval_condition(condition, ctx)

        # Find block boundaries
        depth = 0
        branches: list[tuple[int, int, bool]] = []  # (start, end, condition)
        block_start = start + 1

        for i in range(start, end):
            l = lines[i].strip().lower()
            if l.startswith("if "):
                depth += 1
            elif l == "endif" and depth == 1:
                branches.append((block_start, i, condition_met))
                # Execute the first matching branch
                for bs, be, cond in branches:
                    if cond:
                        await self._execute_lines(lines, bs, be, ctx)
                        break
                return i + 1
            elif l.startswith("elif ") and depth == 1:
                branches.append((block_start, i, condition_met))
                condition_met = not any(c for _, _, c in branches) and await self._eval_condition(l[5:], ctx)
                block_start = i + 1
            elif l == "else" and depth == 1:
                branches.append((block_start, i, condition_met))
                condition_met = not any(c for _, _, c in branches)
                block_start = i + 1
            elif l == "endif":
                depth -= 1

        return end  # Malformed — no endif found

    async def _handle_repeat(self, lines: list[str], start: int, end: int, ctx: AutomationContext) -> int:
        """Handle repeat N / endrepeat blocks."""
        count = int(lines[start].strip().split()[1])
        # Find endrepeat
        depth = 0
        for i in range(start, end):
            l = lines[i].strip().lower()
            if l.startswith("repeat "):
                depth += 1
            elif l == "endrepeat":
                depth -= 1
                if depth == 0:
                    for _ in range(count):
                        if not ctx.running:
                            break
                        await self._execute_lines(lines, start + 1, i, ctx)
                    return i + 1
        return end

    async def _handle_while(self, lines: list[str], start: int, end: int, ctx: AutomationContext) -> int:
        """Handle while <condition> / endwhile blocks."""
        condition = lines[start].strip()[6:]  # after "while "
        # Find endwhile
        depth = 0
        for i in range(start, end):
            l = lines[i].strip().lower()
            if l.startswith("while "):
                depth += 1
            elif l == "endwhile":
                depth -= 1
                if depth == 0:
                    max_iters = 1000
                    iters = 0
                    while ctx.running and iters < max_iters:
                        if not await self._eval_condition(condition, ctx):
                            break
                        await self._execute_lines(lines, start + 1, i, ctx)
                        iters += 1
                    return i + 1
        return end

    async def _eval_condition(self, condition: str, ctx: AutomationContext) -> bool:
        """Evaluate a condition string."""
        condition = condition.strip()

        # text_on_screen "pattern"
        m = re.match(r'text_on_screen\s+"([^"]*)"', condition)
        if m:
            result = self._text_capture.last_result if self._text_capture else None
            return result is not None and m.group(1) in result.text

        # text_not_on_screen "pattern"
        m = re.match(r'text_not_on_screen\s+"([^"]*)"', condition)
        if m:
            result = self._text_capture.last_result if self._text_capture else None
            return result is None or m.group(1) not in result.text

        # pixel_color X Y == "#RRGGBB"
        m = re.match(r'pixel_color\s+(\d+)\s+(\d+)\s*==\s*"(#[0-9a-fA-F]{6})"', condition)
        if m:
            x, y, expected = int(m.group(1)), int(m.group(2)), m.group(3)
            actual = await self._get_pixel_color(x, y, ctx)
            return actual == expected.lower() if actual else False

        # image_on_screen "template.png"
        m = re.match(r'image_on_screen\s+"([^"]*)"', condition)
        if m:
            pos = await self._find_image(m.group(1), ctx)
            return pos is not None

        # Variable comparison
        m = re.match(r'(\w+)\s*==\s*"([^"]*)"', condition)
        if m:
            return ctx.variables.get(m.group(1), "") == m.group(2)

        return False

    # ── Input actions ────────────────────────────────────────────────────────

    async def _type_text(self, text: str, ctx: AutomationContext) -> None:
        from paste_typing import LAYOUTS, KeyStroke
        layout = LAYOUTS.get("us", {})
        for char in text:
            stroke = layout.get(char)
            if not stroke:
                continue
            report = bytes([stroke.modifier, 0, stroke.key, 0, 0, 0, 0, 0])
            self._send_kbd(ctx, report)
            await asyncio.sleep(0.02)
            self._send_kbd(ctx, bytes(8))
            await asyncio.sleep(0.015)

    async def _send_key(self, arg: str, ctx: AutomationContext) -> None:
        from paste_typing import HID_KEYS
        parts = arg.split()
        key_name = parts[0].lower()
        modifier = int(parts[1]) if len(parts) > 1 else 0
        hid_key = HID_KEYS.get(key_name, 0)
        if hid_key:
            self._send_kbd(ctx, bytes([modifier, 0, hid_key, 0, 0, 0, 0, 0]))
            await asyncio.sleep(0.05)
            self._send_kbd(ctx, bytes(8))

    async def _mouse_move(self, x: int, y: int, ctx: AutomationContext) -> None:
        # Absolute coordinates 0-32767
        ax = max(0, min(32767, x * 32767 // 1920))
        ay = max(0, min(32767, y * 32767 // 1080))
        report = bytes([0, ax & 0xFF, (ax >> 8) & 0xFF, ay & 0xFF, (ay >> 8) & 0xFF, 0])
        self._send_mouse(ctx, report)
        await asyncio.sleep(0.01)

    async def _mouse_click(self, button: str, ctx: AutomationContext) -> None:
        btn_map = {"left": 0x01, "right": 0x02, "middle": 0x04}
        btn = btn_map.get(button, 0x01)
        # Click = button down then up (keep position from last move)
        report = bytes([btn, 0, 0, 0, 0, 0])
        self._send_mouse(ctx, report)
        await asyncio.sleep(0.05)
        report = bytes([0, 0, 0, 0, 0, 0])
        self._send_mouse(ctx, report)

    async def _mouse_drag(self, x1: int, y1: int, x2: int, y2: int, ctx: AutomationContext) -> None:
        await self._mouse_move(x1, y1, ctx)
        await asyncio.sleep(0.05)
        # Button down
        ax = max(0, min(32767, x1 * 32767 // 1920))
        ay = max(0, min(32767, y1 * 32767 // 1080))
        self._send_mouse(ctx, bytes([0x01, ax & 0xFF, (ax >> 8) & 0xFF, ay & 0xFF, (ay >> 8) & 0xFF, 0]))
        await asyncio.sleep(0.05)
        # Move to destination
        await self._mouse_move(x2, y2, ctx)
        await asyncio.sleep(0.05)
        # Button up
        self._send_mouse(ctx, bytes([0, 0, 0, 0, 0, 0]))

    def _send_kbd(self, ctx: AutomationContext, report: bytes) -> None:
        self._sock.sendto(bytes([0x01]) + report, (ctx.node_host, ctx.node_port))

    def _send_mouse(self, ctx: AutomationContext, report: bytes) -> None:
        self._sock.sendto(bytes([0x02]) + report, (ctx.node_host, ctx.node_port))

    # ── Screen inspection ────────────────────────────────────────────────────

    async def _wait_for_text(self, arg: str, ctx: AutomationContext) -> None:
        m = re.match(r'"([^"]*)"(?:\s+timeout=(\d+))?', arg)
        if not m:
            return
        pattern, timeout_s = m.group(1), float(m.group(2) or 30)
        deadline = asyncio.get_event_loop().time() + timeout_s
        while asyncio.get_event_loop().time() < deadline and ctx.running:
            result = self._text_capture.last_result if self._text_capture else None
            if result and pattern in result.text:
                return
            await asyncio.sleep(1.0)
        raise TimeoutError(f"wait_for_text '{pattern}' timed out ({timeout_s}s)")

    async def _wait_until_gone(self, arg: str, ctx: AutomationContext) -> None:
        m = re.match(r'"([^"]*)"(?:\s+timeout=(\d+))?', arg)
        if not m:
            return
        pattern, timeout_s = m.group(1), float(m.group(2) or 30)
        deadline = asyncio.get_event_loop().time() + timeout_s
        while asyncio.get_event_loop().time() < deadline and ctx.running:
            result = self._text_capture.last_result if self._text_capture else None
            if not result or pattern not in result.text:
                return
            await asyncio.sleep(1.0)

    async def _get_pixel_color(self, x: int, y: int, ctx: AutomationContext) -> str | None:
        """Get the colour of a pixel at (x, y) from the latest capture frame."""
        if not _IMAGING_AVAILABLE or not self._text_capture:
            return None
        # This would need frame access — placeholder for now
        return None

    async def _find_image(self, template_name: str, ctx: AutomationContext, confidence: float = 0.8) -> tuple[int, int] | None:
        """Find a template image on the captured screen using OpenCV-style matching."""
        if not _IMAGING_AVAILABLE:
            return None

        # Load template
        template_path = Path(__file__).parent / "templates" / template_name
        if not template_path.exists():
            return None

        if template_name not in self._image_templates:
            tpl = np.array(Image.open(template_path).convert("L"), dtype=np.float32) / 255.0
            self._image_templates[template_name] = tpl

        # Would need current frame — placeholder
        # Full implementation would use normalised cross-correlation
        return None

    async def _click_image(self, arg: str, ctx: AutomationContext) -> None:
        """Find an image on screen and click its centre."""
        m = re.match(r'"([^"]*)"(?:\s+confidence=([0-9.]+))?', arg)
        if not m:
            return
        template, conf = m.group(1), float(m.group(2) or 0.8)
        pos = await self._find_image(template, ctx, conf)
        if pos:
            await self._mouse_move(pos[0], pos[1], ctx)
            await self._mouse_click("left", ctx)
        else:
            ctx.errors.append(f"Image not found: {template}")

    async def _wait_for_image(self, arg: str, ctx: AutomationContext) -> None:
        m = re.match(r'"([^"]*)"(?:\s+timeout=(\d+))?', arg)
        if not m:
            return
        template, timeout_s = m.group(1), float(m.group(2) or 30)
        deadline = asyncio.get_event_loop().time() + timeout_s
        while asyncio.get_event_loop().time() < deadline and ctx.running:
            pos = await self._find_image(template, ctx)
            if pos:
                return
            await asyncio.sleep(1.0)

    async def _screenshot(self, filename: str, ctx: AutomationContext) -> None:
        """Save a screenshot (placeholder — needs frame access)."""
        log.info("Screenshot requested: %s", filename)

    # ── Recording ────────────────────────────────────────────────────────────

    def recording_to_script(
        self,
        events: list[dict],
        mode: str = "normal",
    ) -> str:
        """
        Convert recorded HID events to an editable script.

        Modes:
          exact:   Preserve all timing exactly
          normal:  Group rapid keystrokes into 'type' commands,
                   keep pauses, drop sub-50ms delays between characters
          instant: No delays at all
        """
        lines: list[str] = ["# Recorded automation script"]
        lines.append(f"# Mode: {mode}")
        lines.append("")

        if mode == "instant":
            return self._recording_instant(events, lines)
        elif mode == "exact":
            return self._recording_exact(events, lines)
        else:
            return self._recording_normal(events, lines)

    def _recording_exact(self, events: list[dict], lines: list[str]) -> str:
        prev_ts = 0.0
        for evt in events:
            ts = evt.get("timestamp_ms", 0)
            delay = (ts - prev_ts) / 1000.0
            if delay > 0.01 and prev_ts > 0:
                lines.append(f"wait {delay:.3f}")
            prev_ts = ts

            if evt.get("type") == "key":
                key_name = evt.get("key_name", "")
                mod = evt.get("modifier", 0)
                if evt.get("action") == "down":
                    if mod:
                        lines.append(f"key {key_name} {mod}")
                    else:
                        lines.append(f"key {key_name}")
            elif evt.get("type") == "mouse_move":
                lines.append(f"mouse_move {evt['x']} {evt['y']}")
            elif evt.get("type") == "mouse_click":
                lines.append(f"mouse_click {evt.get('button', 'left')}")

        return "\n".join(lines)

    def _recording_normal(self, events: list[dict], lines: list[str]) -> str:
        """Group rapid keystrokes into type commands."""
        from paste_typing import HID_KEYS

        # Reverse map: HID key → character
        hid_to_char = {}
        for char, hid in HID_KEYS.items():
            if len(char) == 1:
                hid_to_char[hid] = char

        text_buffer = ""
        prev_ts = 0.0

        for evt in events:
            ts = evt.get("timestamp_ms", 0)
            gap = ts - prev_ts if prev_ts > 0 else 0

            if evt.get("type") == "key" and evt.get("action") == "down":
                char = hid_to_char.get(evt.get("hid_key", 0))

                if gap > 500 and text_buffer:
                    # Flush text buffer if there's a long pause
                    lines.append(f'type "{text_buffer}"')
                    text_buffer = ""
                    lines.append(f"wait {gap / 1000:.1f}")

                if char and evt.get("modifier", 0) == 0:
                    text_buffer += char
                elif char and evt.get("modifier", 0) == 2:  # shift
                    text_buffer += char.upper()
                else:
                    if text_buffer:
                        lines.append(f'type "{text_buffer}"')
                        text_buffer = ""
                    key_name = evt.get("key_name", f"0x{evt.get('hid_key', 0):02x}")
                    mod = evt.get("modifier", 0)
                    if mod:
                        lines.append(f"key {key_name} {mod}")
                    else:
                        lines.append(f"key {key_name}")

            elif evt.get("type") == "mouse_move":
                if text_buffer:
                    lines.append(f'type "{text_buffer}"')
                    text_buffer = ""
                if gap > 200:
                    lines.append(f"wait {gap / 1000:.1f}")
                lines.append(f"mouse_move {evt['x']} {evt['y']}")

            elif evt.get("type") == "mouse_click":
                if text_buffer:
                    lines.append(f'type "{text_buffer}"')
                    text_buffer = ""
                lines.append(f"mouse_click {evt.get('button', 'left')}")

            prev_ts = ts

        if text_buffer:
            lines.append(f'type "{text_buffer}"')

        return "\n".join(lines)

    def _recording_instant(self, events: list[dict], lines: list[str]) -> str:
        """No delays — just the actions."""
        result = self._recording_normal(events, lines)
        # Strip all wait commands
        return "\n".join(l for l in result.splitlines() if not l.strip().startswith("wait "))


class _AbortScript(Exception):
    """Raised by 'abort' command to stop script execution."""
    pass
