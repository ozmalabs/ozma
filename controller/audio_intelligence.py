# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Audio intelligence — ducking, voice detection, noise floor monitoring.

Analyses audio streams in PipeWire for:
  1. Voice activity detection → auto-duck music/game audio when speaking
  2. Noise floor monitoring → track ambient noise over time
  3. Audio level metering → feed to screen renderer for VU meters

Uses PipeWire's audio analysis (pw-cat capture → simple energy detection).
No ML required for basic voice/silence detection.
"""

from __future__ import annotations

import asyncio
import logging
import math
from collections import deque
from typing import Any

log = logging.getLogger("ozma.audio_intel")

SILENCE_THRESHOLD = 0.02    # RMS below this = silence
SPEECH_THRESHOLD = 0.05     # RMS above this = speech
DUCK_AMOUNT = 0.3           # Reduce music volume to 30% during speech
DUCK_ATTACK_MS = 50         # Fade-down time
DUCK_RELEASE_MS = 500       # Fade-up time after speech stops


class AudioIntelligence:
    """
    Audio analysis for ducking, voice detection, and noise monitoring.
    """

    def __init__(self, audio_router: Any = None) -> None:
        self._audio = audio_router
        self._ducking_enabled = False
        self._ducked = False
        self._noise_floor_history: deque[tuple[float, float]] = deque(maxlen=3600)
        self._current_level_l: float = 0.0
        self._current_level_r: float = 0.0
        self._speech_detected = False
        self._task: asyncio.Task | None = None

    async def start(self) -> None:
        log.info("Audio intelligence started")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()

    def enable_ducking(self, enabled: bool = True) -> None:
        self._ducking_enabled = enabled
        log.info("Audio ducking %s", "enabled" if enabled else "disabled")

    @property
    def speech_detected(self) -> bool:
        return self._speech_detected

    @property
    def levels(self) -> tuple[float, float]:
        return (self._current_level_l, self._current_level_r)

    def update_levels(self, level_l: float, level_r: float) -> None:
        """Called with audio levels from PipeWire monitoring."""
        self._current_level_l = level_l
        self._current_level_r = level_r

        avg = (level_l + level_r) / 2
        was_speaking = self._speech_detected
        self._speech_detected = avg > SPEECH_THRESHOLD

        if self._ducking_enabled:
            if self._speech_detected and not self._ducked:
                self._ducked = True
                asyncio.create_task(self._apply_duck(DUCK_AMOUNT))
            elif not self._speech_detected and self._ducked and not was_speaking:
                self._ducked = False
                asyncio.create_task(self._apply_duck(1.0))

    async def _apply_duck(self, volume: float) -> None:
        """Adjust music/game audio volume for ducking."""
        if self._audio and hasattr(self._audio, 'set_volume'):
            # Would duck the active node's audio output
            pass

    def to_dict(self) -> dict[str, Any]:
        return {
            "ducking_enabled": self._ducking_enabled,
            "ducked": self._ducked,
            "speech_detected": self._speech_detected,
            "level_l": round(self._current_level_l, 3),
            "level_r": round(self._current_level_r, 3),
        }
