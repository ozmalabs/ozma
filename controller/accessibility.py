# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Accessibility features — hardware screen reader and input assistance.

Provides accessibility for machines that can't run their own accessibility
tools: BIOS screens, locked-down systems, embedded devices, machines
with no OS.

Features:
  Screen reader  — OCR the captured display → text-to-speech via PipeWire
  Key echo       — speak each key as it's typed (via HID log)
  Navigation     — describe screen layout from OCR
  Alerts         — audible alerts for OCR trigger matches

Uses espeak-ng or festival for TTS.  Audio output through PipeWire to
the operator's headphones/speakers.
"""

from __future__ import annotations

import asyncio
import logging
import shutil
from typing import Any

log = logging.getLogger("ozma.accessibility")


class HardwareScreenReader:
    """
    OCR-based screen reader that works on any captured display.

    Reads the screen via OCR and speaks the text through the controller's
    audio output.  Works on BIOS, DOS, Linux console, locked systems —
    anything the capture card can see.
    """

    def __init__(self, text_capture: Any = None) -> None:
        self._text_capture = text_capture
        self._enabled = False
        self._tts_engine = self._detect_tts()
        self._last_spoken = ""
        self._task: asyncio.Task | None = None
        self._speak_rate = 175  # words per minute
        self._voice = "en"

    @property
    def available(self) -> bool:
        return self._tts_engine is not None

    async def start(self) -> None:
        if not self._tts_engine:
            log.info("No TTS engine found — screen reader disabled (install espeak-ng)")
            return
        self._enabled = True
        self._task = asyncio.create_task(self._read_loop(), name="screen-reader")
        log.info("Hardware screen reader started (engine: %s)", self._tts_engine)

    async def stop(self) -> None:
        self._enabled = False
        if self._task:
            self._task.cancel()

    async def speak(self, text: str) -> None:
        """Speak text through the controller's audio output."""
        if not self._tts_engine or not text.strip():
            return
        try:
            proc = await asyncio.create_subprocess_exec(
                self._tts_engine, "-s", str(self._speak_rate), "-v", self._voice, text[:500],
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.DEVNULL,
            )
            await asyncio.wait_for(proc.wait(), timeout=30)
        except Exception:
            pass

    async def speak_screen(self) -> str:
        """OCR the current screen and speak it."""
        if not self._text_capture:
            return ""
        result = self._text_capture.last_result
        if not result or not result.text:
            return ""
        text = result.text.strip()
        if text != self._last_spoken:
            self._last_spoken = text
            await self.speak(text)
        return text

    async def speak_changes(self) -> str:
        """Speak only the lines that changed since last read."""
        if not self._text_capture:
            return ""
        result = self._text_capture.last_result
        if not result:
            return ""

        new_text = result.text.strip()
        if new_text == self._last_spoken:
            return ""

        # Find changed lines
        old_lines = set(self._last_spoken.splitlines())
        new_lines = new_text.splitlines()
        changed = [l for l in new_lines if l.strip() and l not in old_lines]

        self._last_spoken = new_text
        if changed:
            delta = "\n".join(changed[:10])
            await self.speak(delta)
            return delta
        return ""

    async def _read_loop(self) -> None:
        """Periodically read screen changes."""
        while self._enabled:
            try:
                await self.speak_changes()
                await asyncio.sleep(3.0)  # Check every 3 seconds
            except asyncio.CancelledError:
                return

    def _detect_tts(self) -> str | None:
        """Find an available TTS engine."""
        for engine in ["espeak-ng", "espeak", "festival", "say"]:  # 'say' = macOS
            if shutil.which(engine):
                return engine
        return None

    def to_dict(self) -> dict[str, Any]:
        return {
            "enabled": self._enabled,
            "available": self.available,
            "tts_engine": self._tts_engine,
            "speak_rate": self._speak_rate,
            "voice": self._voice,
        }
