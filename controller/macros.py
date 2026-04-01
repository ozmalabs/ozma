# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Macro recording, playback, and scripting for ozma.

Three levels of automation:

1. **Macro recording** — record HID keystrokes with timestamps, replay them.
   Good for repeating a sequence of key presses (e.g., navigate BIOS menus).

2. **Macro library** — named macros stored in macros.json, triggered via
   API, control surfaces, or the dashboard.

3. **Scripting DSL** — a simple automation language combining OCR + typing + timing:
     wait_for_text "login:"
     type "root\\n"
     wait 2
     type "password123\\n"
     wait_for_text "$"
     key f5

Script commands:
  type <text>              — type text via paste-typing
  key <name> [modifier]    — send a single key (enter, f1, esc, etc.)
  wait <seconds>           — pause
  wait_for_text <pattern>  — OCR the screen repeatedly until pattern appears
  scenario <id>            — switch scenario
  note <text>              — add an RGB notification
  log <text>               — log a message
"""

from __future__ import annotations

import asyncio
import json
import logging
import re
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.macros")


@dataclass
class MacroStep:
    """A single step in a recorded macro."""
    timestamp_ms: float    # ms since macro start
    action: str            # "key_down", "key_up"
    hid_key: int = 0
    modifier: int = 0


@dataclass
class Macro:
    """A named, replayable macro."""
    id: str
    name: str
    steps: list[MacroStep] = field(default_factory=list)
    created: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "steps": len(self.steps),
            "duration_ms": self.steps[-1].timestamp_ms if self.steps else 0,
        }


class MacroManager:
    """Records, stores, and replays keyboard macros."""

    def __init__(self, state: Any, paste_typer: Any, text_capture: Any = None) -> None:
        self._state = state
        self._paste_typer = paste_typer
        self._text_capture = text_capture
        self._macros: dict[str, Macro] = {}
        self._recording: Macro | None = None
        self._recording_start: float = 0.0
        self._playing = False
        self._macros_path = Path(__file__).parent / "macros.json"
        self._load()

    @property
    def is_recording(self) -> bool:
        return self._recording is not None

    @property
    def is_playing(self) -> bool:
        return self._playing

    # ── Recording ────────────────────────────────────────────────────────────

    def start_recording(self, macro_id: str, name: str = "") -> None:
        self._recording = Macro(id=macro_id, name=name or macro_id, created=time.time())
        self._recording_start = time.monotonic()
        log.info("Macro recording started: %s", macro_id)

    def record_keystroke(self, action: str, hid_key: int, modifier: int = 0) -> None:
        if not self._recording:
            return
        elapsed = (time.monotonic() - self._recording_start) * 1000
        self._recording.steps.append(MacroStep(
            timestamp_ms=elapsed, action=action,
            hid_key=hid_key, modifier=modifier,
        ))

    def stop_recording(self) -> Macro | None:
        if not self._recording:
            return None
        macro = self._recording
        self._recording = None
        self._macros[macro.id] = macro
        self._save()
        log.info("Macro recorded: %s (%d steps, %.0fms)",
                 macro.id, len(macro.steps),
                 macro.steps[-1].timestamp_ms if macro.steps else 0)
        return macro

    # ── Playback ─────────────────────────────────────────────────────────────

    async def play(self, macro_id: str, node_id: str | None = None) -> bool:
        macro = self._macros.get(macro_id)
        if not macro or not macro.steps:
            return False

        node = self._state.nodes.get(node_id) if node_id else self._state.get_active_node()
        if not node:
            return False

        self._playing = True
        import socket
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

        try:
            prev_ts = 0.0
            for step in macro.steps:
                delay = (step.timestamp_ms - prev_ts) / 1000.0
                if delay > 0:
                    await asyncio.sleep(delay)
                prev_ts = step.timestamp_ms

                if step.action == "key_down":
                    report = bytes([step.modifier, 0, step.hid_key, 0, 0, 0, 0, 0])
                else:
                    report = bytes(8)
                packet = bytes([0x01]) + report
                sock.sendto(packet, (node.host, node.port))
            return True
        finally:
            sock.close()
            self._playing = False

    # ── Scripting DSL ────────────────────────────────────────────────────────

    async def run_script(self, script: str, node_id: str | None = None) -> dict[str, Any]:
        """
        Execute a macro script.

        Commands:
          type <text>              — type text via paste-typing
          key <name> [modifier]    — single key
          wait <seconds>           — pause
          wait_for_text <pattern>  — OCR until pattern found (timeout 30s)
          scenario <id>            — switch scenario
          note <text>              — RGB notification
          log <text>               — log message
        """
        lines_executed = 0
        errors = []

        for line_num, raw_line in enumerate(script.splitlines(), 1):
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue

            try:
                await self._exec_line(line, node_id)
                lines_executed += 1
            except Exception as e:
                errors.append(f"Line {line_num}: {e}")
                log.warning("Script error line %d: %s — %s", line_num, line, e)

        return {"ok": not errors, "lines_executed": lines_executed, "errors": errors}

    async def _exec_line(self, line: str, node_id: str | None) -> None:
        parts = line.split(None, 1)
        cmd = parts[0].lower()
        arg = parts[1] if len(parts) > 1 else ""

        match cmd:
            case "type":
                text = arg.replace("\\n", "\n").replace("\\t", "\t")
                await self._paste_typer.type_text(text, node_id=node_id)

            case "key":
                key_parts = arg.split()
                key_name = key_parts[0] if key_parts else ""
                modifier = int(key_parts[1]) if len(key_parts) > 1 else 0
                await self._paste_typer.type_key(key_name, modifier=modifier, node_id=node_id)

            case "wait":
                await asyncio.sleep(float(arg))

            case "wait_for_text":
                pattern = arg.strip('"').strip("'")
                await self._wait_for_text(pattern)

            case "scenario":
                # Import here to avoid circular dependency
                pass  # Caller should wire this up via callback

            case "note":
                log.info("Script note: %s", arg)

            case "log":
                log.info("Script: %s", arg)

            case _:
                raise ValueError(f"Unknown command: {cmd}")

    async def _wait_for_text(self, pattern: str, timeout: float = 30.0) -> None:
        """OCR the screen repeatedly until pattern is found."""
        if not self._text_capture:
            raise RuntimeError("No text capture available for wait_for_text")

        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            result = self._text_capture.last_result
            if result and pattern in result.text:
                return
            await asyncio.sleep(1.0)
        raise TimeoutError(f"Pattern '{pattern}' not found within {timeout}s")

    # ── Library ──────────────────────────────────────────────────────────────

    def list_macros(self) -> list[dict[str, Any]]:
        return [m.to_dict() for m in self._macros.values()]

    def delete_macro(self, macro_id: str) -> bool:
        if macro_id in self._macros:
            del self._macros[macro_id]
            self._save()
            return True
        return False

    def _save(self) -> None:
        data = {}
        for mid, macro in self._macros.items():
            data[mid] = {
                "name": macro.name,
                "created": macro.created,
                "steps": [
                    {"ts": s.timestamp_ms, "action": s.action,
                     "key": s.hid_key, "mod": s.modifier}
                    for s in macro.steps
                ],
            }
        try:
            self._macros_path.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.warning("Failed to save macros: %s", e)

    def _load(self) -> None:
        if not self._macros_path.exists():
            return
        try:
            data = json.loads(self._macros_path.read_text())
            for mid, mdata in data.items():
                steps = [
                    MacroStep(timestamp_ms=s["ts"], action=s["action"],
                              hid_key=s["key"], modifier=s.get("mod", 0))
                    for s in mdata.get("steps", [])
                ]
                self._macros[mid] = Macro(
                    id=mid, name=mdata.get("name", mid),
                    steps=steps, created=mdata.get("created", 0),
                )
        except Exception as e:
            log.warning("Failed to load macros: %s", e)
