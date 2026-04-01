# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Spatial RGB effect engine — keyboard edition.

Evaluates f(key_pos_mm, t_ms) → RGB for each key on an ANSI TKL layout.

Key IDs match browser KeyboardEvent.code strings so the UI can apply colors
directly with document.getElementById(keyId).

Effect: wave_plane
  A color boundary sweeps left→right across the keyboard. Wave speed is
  calculated from the scene extent so the full sweep always takes exactly
  duration_ms — physically coherent across any layout width.

  With a soft blade_mm blend zone around the wave front, you get a smooth
  gradient rather than a hard cut.

Design note (spatial compression):
  This engine always compresses to scene extent. wave speed =
  keyboard_width_mm / duration_ms so the furthest key (rightmost) reaches
  the new color exactly at t=duration_ms. A physically accurate mm/ms speed
  would leave most of the keyboard in the old color for the majority of the
  effect duration — compression ensures every key participates dramatically.
"""

import math
from typing import NamedTuple

RGB = tuple[int, int, int]


# ---------------------------------------------------------------------------
# ANSI TKL key layout
# Physical positions in mm from the top-left corner of the keyboard.
# 1U = 19.05 mm.  Layout is approximate but proportionally accurate.
# ---------------------------------------------------------------------------

_U = 19.05  # 1 key unit in mm


def _u(n: float) -> float:
    return n * _U


# (x_center_mm, y_center_mm) for each key, using KeyboardEvent.code strings.
# Origin = top-left corner of keyboard housing.

KEY_POSITIONS: dict[str, tuple[float, float]] = {
    # ── Fn row (y = 9.5) ──────────────────────────────────────────────────
    "Escape":      (_u(0.5),   _u(0.5)),

    "F1":          (_u(2.0),   _u(0.5)),   # small gap after Escape
    "F2":          (_u(3.0),   _u(0.5)),
    "F3":          (_u(4.0),   _u(0.5)),
    "F4":          (_u(5.0),   _u(0.5)),

    "F5":          (_u(6.25),  _u(0.5)),   # gap between groups
    "F6":          (_u(7.25),  _u(0.5)),
    "F7":          (_u(8.25),  _u(0.5)),
    "F8":          (_u(9.25),  _u(0.5)),

    "F9":          (_u(10.5),  _u(0.5)),
    "F10":         (_u(11.5),  _u(0.5)),
    "F11":         (_u(12.5),  _u(0.5)),
    "F12":         (_u(13.5),  _u(0.5)),

    # ── Number row (y = 1.75U) ────────────────────────────────────────────
    "Backquote":   (_u(0.5),   _u(1.75)),
    "Digit1":      (_u(1.5),   _u(1.75)),
    "Digit2":      (_u(2.5),   _u(1.75)),
    "Digit3":      (_u(3.5),   _u(1.75)),
    "Digit4":      (_u(4.5),   _u(1.75)),
    "Digit5":      (_u(5.5),   _u(1.75)),
    "Digit6":      (_u(6.5),   _u(1.75)),
    "Digit7":      (_u(7.5),   _u(1.75)),
    "Digit8":      (_u(8.5),   _u(1.75)),
    "Digit9":      (_u(9.5),   _u(1.75)),
    "Digit0":      (_u(10.5),  _u(1.75)),
    "Minus":       (_u(11.5),  _u(1.75)),
    "Equal":       (_u(12.5),  _u(1.75)),
    "Backspace":   (_u(13.75), _u(1.75)),  # 2U key

    # ── Tab row (y = 2.75U) ───────────────────────────────────────────────
    "Tab":         (_u(0.75),  _u(2.75)),  # 1.5U
    "KeyQ":        (_u(1.75),  _u(2.75)),
    "KeyW":        (_u(2.75),  _u(2.75)),
    "KeyE":        (_u(3.75),  _u(2.75)),
    "KeyR":        (_u(4.75),  _u(2.75)),
    "KeyT":        (_u(5.75),  _u(2.75)),
    "KeyY":        (_u(6.75),  _u(2.75)),
    "KeyU":        (_u(7.75),  _u(2.75)),
    "KeyI":        (_u(8.75),  _u(2.75)),
    "KeyO":        (_u(9.75),  _u(2.75)),
    "KeyP":        (_u(10.75), _u(2.75)),
    "BracketLeft": (_u(11.75), _u(2.75)),
    "BracketRight":(_u(12.75), _u(2.75)),
    "Backslash":   (_u(13.75), _u(2.75)),  # 1.5U (ISO: Enter here)

    # ── Caps row (y = 3.75U) ──────────────────────────────────────────────
    "CapsLock":    (_u(0.875), _u(3.75)),  # 1.75U
    "KeyA":        (_u(1.875), _u(3.75)),
    "KeyS":        (_u(2.875), _u(3.75)),
    "KeyD":        (_u(3.875), _u(3.75)),
    "KeyF":        (_u(4.875), _u(3.75)),
    "KeyG":        (_u(5.875), _u(3.75)),
    "KeyH":        (_u(6.875), _u(3.75)),
    "KeyJ":        (_u(7.875), _u(3.75)),
    "KeyK":        (_u(8.875), _u(3.75)),
    "KeyL":        (_u(9.875), _u(3.75)),
    "Semicolon":   (_u(10.875),_u(3.75)),
    "Quote":       (_u(11.875),_u(3.75)),
    "Enter":       (_u(13.125),_u(3.75)),  # 2.25U

    # ── Shift row (y = 4.75U) ─────────────────────────────────────────────
    "ShiftLeft":   (_u(1.125), _u(4.75)),  # 2.25U
    "KeyZ":        (_u(2.5),   _u(4.75)),
    "KeyX":        (_u(3.5),   _u(4.75)),
    "KeyC":        (_u(4.5),   _u(4.75)),
    "KeyV":        (_u(5.5),   _u(4.75)),
    "KeyB":        (_u(6.5),   _u(4.75)),
    "KeyN":        (_u(7.5),   _u(4.75)),
    "KeyM":        (_u(8.5),   _u(4.75)),
    "Comma":       (_u(9.5),   _u(4.75)),
    "Period":      (_u(10.5),  _u(4.75)),
    "Slash":       (_u(11.5),  _u(4.75)),
    "ShiftRight":  (_u(13.125),_u(4.75)),  # 2.75U

    # ── Bottom row (y = 5.75U) ────────────────────────────────────────────
    "ControlLeft": (_u(0.625), _u(5.75)),  # 1.25U
    "MetaLeft":    (_u(1.875), _u(5.75)),  # 1.25U
    "AltLeft":     (_u(3.125), _u(5.75)),  # 1.25U
    "Space":       (_u(7.375), _u(5.75)),  # 6.25U center
    "AltRight":    (_u(11.625),_u(5.75)),  # 1.25U
    "MetaRight":   (_u(12.875),_u(5.75)),  # 1.25U
    "ContextMenu": (_u(13.125),_u(5.75)),  # 1U
    "ControlRight":(_u(14.125),_u(5.75)),  # 1.25U

    # ── Navigation cluster (offset +15.25U from left edge) ────────────────
    "Insert":      (_u(15.75), _u(1.75)),
    "Home":        (_u(16.75), _u(1.75)),
    "PageUp":      (_u(17.75), _u(1.75)),
    "Delete":      (_u(15.75), _u(2.75)),
    "End":         (_u(16.75), _u(2.75)),
    "PageDown":    (_u(17.75), _u(2.75)),
    "ArrowUp":     (_u(16.75), _u(4.75)),
    "ArrowLeft":   (_u(15.75), _u(5.75)),
    "ArrowDown":   (_u(16.75), _u(5.75)),
    "ArrowRight":  (_u(17.75), _u(5.75)),
}

# Extent of the keyboard in mm (used for scene compression)
_all_x = [pos[0] for pos in KEY_POSITIONS.values()]
KEYBOARD_WIDTH_MM: float = max(_all_x) - min(_all_x)
KEYBOARD_LEFT_MM: float = min(_all_x)


# ---------------------------------------------------------------------------
# Color utilities
# ---------------------------------------------------------------------------

def hex_to_rgb(color: str) -> RGB:
    """Parse '#RRGGBB' or '#RGB' → (r, g, b) 0-255."""
    color = color.lstrip("#")
    if len(color) == 3:
        color = "".join(c * 2 for c in color)
    r = int(color[0:2], 16)
    g = int(color[2:4], 16)
    b = int(color[4:6], 16)
    return (r, g, b)


def lerp_rgb(a: RGB, b: RGB, t: float) -> RGB:
    t = max(0.0, min(1.0, t))
    return (
        round(a[0] + (b[0] - a[0]) * t),
        round(a[1] + (b[1] - a[1]) * t),
        round(a[2] + (b[2] - a[2]) * t),
    )


def _sigmoid(x: float) -> float:
    """Smooth S-curve blend, x in 0-1 range."""
    x = max(0.0, min(1.0, x))
    return x * x * (3 - 2 * x)  # smoothstep


# ---------------------------------------------------------------------------
# Effect engine
# ---------------------------------------------------------------------------

class RGBEngine:
    """
    Evaluates per-key RGB values for the current effect state.

    All effects use scene-extent compression by default: wave speed is
    scaled so the furthest key in the scene reaches the new color exactly
    at t=duration_ms.
    """

    BLADE_MM: float = 30.0  # blend zone width in mm

    def solid(self, color: RGB) -> dict[str, RGB]:
        """All keys one color."""
        return {key: color for key in KEY_POSITIONS}

    def wave_frame(
        self,
        t_ms: float,
        duration_ms: float,
        color_from: RGB,
        color_to: RGB,
    ) -> dict[str, RGB]:
        """
        Left-to-right wave sweep. Scene-compressed so the wave front
        reaches the rightmost key exactly at t=duration_ms.

        t_ms=0           → all keys show color_from
        t_ms=duration_ms → all keys show color_to
        """
        # Wave front traverses from one blade-width before the leftmost key
        # to one blade-width after the rightmost key, so all keys begin
        # fully in color_from and end fully in color_to.
        progress = t_ms / max(duration_ms, 1.0)
        travel_start = KEYBOARD_LEFT_MM - self.BLADE_MM
        travel_end   = KEYBOARD_LEFT_MM + KEYBOARD_WIDTH_MM + self.BLADE_MM
        wave_front_x = travel_start + (travel_end - travel_start) * progress

        result: dict[str, RGB] = {}
        half_blade = self.BLADE_MM / 2.0

        for key, (x, _y) in KEY_POSITIONS.items():
            # Normalised position within the blade zone
            # -1.0 = fully behind wave (color_to), +1.0 = fully ahead (color_from)
            dist = x - wave_front_x
            if dist > half_blade:
                result[key] = color_from
            elif dist < -half_blade:
                result[key] = color_to
            else:
                # Smooth blend across the blade
                t = _sigmoid((half_blade - dist) / self.BLADE_MM)
                result[key] = lerp_rgb(color_from, color_to, t)

        return result


# ---------------------------------------------------------------------------
# Transition runner
# ---------------------------------------------------------------------------

import asyncio


async def run_transition(
    engine: RGBEngine,
    event_queue: asyncio.Queue,
    color_from: RGB,
    color_to: RGB,
    duration_ms: float,
    fps: int = 30,
) -> None:
    """
    Drive a wave_plane transition, putting rgb.frame events onto the queue
    at `fps` for `duration_ms`, then putting a final solid frame.
    """
    frame_interval = 1.0 / fps
    loop = asyncio.get_running_loop()
    t_start = loop.time()
    t_end = t_start + duration_ms / 1000.0

    while True:
        now = loop.time()
        elapsed_ms = (now - t_start) * 1000.0

        if elapsed_ms >= duration_ms:
            # Final frame: all keys at destination color
            frame = engine.solid(color_to)
            await event_queue.put({"type": "rgb.frame", "keys": {k: list(v) for k, v in frame.items()}})
            return

        frame = engine.wave_frame(elapsed_ms, duration_ms, color_from, color_to)
        await event_queue.put({"type": "rgb.frame", "keys": {k: list(v) for k, v in frame.items()}})

        # Sleep until next frame, accounting for time spent computing
        elapsed = loop.time() - now
        sleep_for = max(0.0, frame_interval - elapsed)
        await asyncio.sleep(sleep_for)
