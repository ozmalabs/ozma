# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
MIDI control surface support for ozma.

Ported from surfacepresser-run's midi_controller.py + midi_integration.py,
rewritten as a clean async module that integrates with ozma's ControlSurface
abstraction.

Supports:
  - Faders (motorised, with touch lockout)
  - Buttons (toggle / momentary, with LED feedback)
  - Rotary encoders
  - Jog wheels
  - Behringer X-Touch scribble strip LCD displays
  - Behringer 7-segment displays

Requires: pip install mido python-rtmidi
Optional: pip install unidecode (for LCD Unicode→ASCII fallback)
"""

from __future__ import annotations

import asyncio
import logging
from enum import Enum
from typing import Any, Callable

try:
    import mido
    _MIDO_AVAILABLE = True
except ImportError:
    _MIDO_AVAILABLE = False

try:
    from unidecode import unidecode as _unidecode
except ImportError:
    def _unidecode(s: str) -> str:
        return s.encode("ascii", "replace").decode()

from controls import ControlSurface, Control, ControlBinding, DisplayControl

log = logging.getLogger("ozma.midi")


# ── Enums ────────────────────────────────────────────────────────────────────

class Color(Enum):
    BLACK = 0
    RED = 1
    GREEN = 2
    YELLOW = 3
    BLUE = 4
    MAGENTA = 5
    CYAN = 6
    WHITE = 7


class Invert(Enum):
    NONE = 0
    TOP = 1
    BOTTOM = 2
    BOTH = 3


# Map hex colour to closest LCD colour
_COLOR_MAP = {
    "#ff0000": Color.RED, "#00ff00": Color.GREEN, "#0000ff": Color.BLUE,
    "#ffff00": Color.YELLOW, "#ff00ff": Color.MAGENTA, "#00ffff": Color.CYAN,
    "#ffffff": Color.WHITE, "#000000": Color.BLACK,
}


def _hex_to_lcd_color(hex_color: str | None) -> Color:
    """Best-effort hex colour → LCD Color mapping."""
    if not hex_color:
        return Color.WHITE
    hex_color = hex_color.lower().strip()
    if hex_color in _COLOR_MAP:
        return _COLOR_MAP[hex_color]
    # Try name match
    for c in Color:
        if c.name.lower() in hex_color:
            return c
    return Color.WHITE


# ── 7-segment font (for Behringer segment displays) ─────────────────────────

_7SEG_FONT = {
    "0": 0x3F, "1": 0x06, "2": 0x5B, "3": 0x4F, "4": 0x66, "5": 0x6D,
    "6": 0x7D, "7": 0x07, "8": 0x7F, "9": 0x6F,
    "A": 0x77, "B": 0x7F, "C": 0x39, "D": 0x3F, "E": 0x79, "F": 0x71,
    "G": 0x3D, "H": 0x76, "I": 0x06, "J": 0x0E, "K": 0x75, "L": 0x38,
    "M": 0x37, "N": 0x37, "O": 0x3F, "P": 0x73, "Q": 0x67, "R": 0x77,
    "S": 0x6D, "T": 0x78, "U": 0x3E, "V": 0x3E, "W": 0x3E, "X": 0x49,
    "Y": 0x6E, "Z": 0x5B,
    " ": 0x00, "-": 0x40, ".": 0x08, ":": 0x09, "(": 0x39, ")": 0x0F,
}


def _render_7seg(text: str) -> list[int]:
    return [_7SEG_FONT.get(c.upper(), _7SEG_FONT.get(c, 0)) for c in text]


# ── Low-level MIDI I/O ──────────────────────────────────────────────────────

class MidiIO:
    """
    Low-level MIDI device wrapper using mido.

    Runs mido's input callback in a thread; bridges to asyncio via
    loop.call_soon_threadsafe.
    """

    def __init__(
        self,
        device_name: str,
        on_message: Callable[["mido.Message"], None] | None = None,
    ) -> None:
        self._device_name = device_name
        self._on_message = on_message
        self._port_in: Any = None
        self._port_out: Any = None

    @staticmethod
    def available() -> bool:
        return _MIDO_AVAILABLE

    @staticmethod
    def list_devices() -> list[str]:
        if not _MIDO_AVAILABLE:
            return []
        return list(set(mido.get_input_names()) | set(mido.get_output_names()))

    def open(self) -> None:
        in_name = self._find_port(mido.get_input_names(), self._device_name)
        out_name = self._find_port(mido.get_output_names(), self._device_name)
        self._port_in = mido.open_input(in_name, callback=self._raw_callback)
        self._port_out = mido.open_output(out_name)
        log.info("MIDI opened: in=%s out=%s", in_name, out_name)

    def close(self) -> None:
        if self._port_in:
            self._port_in.close()
        if self._port_out:
            self._port_out.close()

    def send(self, msg: "mido.Message") -> None:
        if self._port_out:
            self._port_out.send(msg)

    def note_on(self, note: int, velocity: int) -> None:
        self.send(mido.Message("note_on", note=note, velocity=velocity))

    def control_change(self, control: int, value: int) -> None:
        self.send(mido.Message("control_change", control=control, value=value))

    def sysex(self, data: list[int]) -> None:
        self.send(mido.Message("sysex", data=data))

    def lcd_update(self, text: str, color: Color = Color.WHITE,
                   invert: Invert = Invert.NONE) -> None:
        """Send Behringer X-Touch scribble strip LCD update (14 chars)."""
        text = _unidecode(text)
        chars = list(map(ord, text[:14]))
        chars = (chars + [0] * 14)[:14]
        color_code = color.value | (invert.value << 4)
        self.sysex([0x00, 0x20, 0x32, 0x41, 0x4C, 0x00, color_code] + chars)

    def segment_update(self, text: str) -> None:
        """Send Behringer 7-segment display update (12 chars)."""
        text = _unidecode(text)
        rendered = (_render_7seg(text[:12]) + [0] * 12)[:12]
        self.sysex([0x00, 0x20, 0x32, 0x41, 0x37] + rendered + [0x00, 0x00])

    def _raw_callback(self, msg: "mido.Message") -> None:
        if self._on_message:
            self._on_message(msg)

    @staticmethod
    def _find_port(names: list[str], pattern: str) -> str:
        for name in names:
            if name.startswith(pattern):
                return name
        raise RuntimeError(f"No MIDI port matching '{pattern}' in {names}")


# ── MIDI Control classes ─────────────────────────────────────────────────────

class MidiControl:
    """Base class for a physical MIDI control (fader, button, etc.)."""

    def __init__(self, name: str, config: dict, midi: MidiIO) -> None:
        self.name = name
        self._midi = midi
        self._config = config
        self.value: Any = 0
        self.lockout: bool = False

    def on_midi_message(self, msg: "mido.Message") -> dict | None:
        """Process incoming MIDI message, return state delta dict or None."""
        return None

    def set_value(self, value: Any) -> None:
        """Set from external source (feedback path). Respects lockout."""
        pass


class MidiFader(MidiControl):
    """Motorised fader with touch detection (lockout while touched)."""

    def __init__(self, name: str, config: dict, midi: MidiIO) -> None:
        super().__init__(name, config, midi)
        self.value = 0
        self._cc = config.get("control", 70)
        self._touch_note = config.get("note")

    def on_midi_message(self, msg: "mido.Message") -> dict | None:
        if msg.type == "control_change" and msg.control == self._cc:
            self.value = msg.value
            return {"value": msg.value}
        if (self._touch_note is not None and msg.type == "note_on"
                and msg.note == self._touch_note):
            self.lockout = msg.velocity >= 64
            return {"lockout": self.lockout}
        return None

    def set_value(self, value: Any) -> None:
        if not self.lockout:
            v = max(0, min(127, int(value)))
            self.value = v
            self._midi.control_change(self._cc, v)


class MidiButton(MidiControl):
    """Button with LED, supports toggle and momentary modes."""

    def __init__(self, name: str, config: dict, midi: MidiIO) -> None:
        super().__init__(name, config, midi)
        self._note = config.get("note", 0)
        self._style = config.get("style", "toggle")  # toggle | momentary
        self._light_style = config.get("light", "state")  # state | always_on | momentary | False
        self.value = False
        self.pressed = False
        self._update_light()

    def on_midi_message(self, msg: "mido.Message") -> dict | None:
        if msg.type != "note_on" or msg.note != self._note:
            return None
        if msg.velocity >= 64:  # press
            self.pressed = True
            if self._style == "toggle":
                self.value = not self.value
            else:
                self.value = True
        else:  # release
            self.pressed = False
            if self._style == "momentary":
                self.value = False
        self._update_light()
        return {"value": self.value, "pressed": self.pressed}

    def set_value(self, value: Any) -> None:
        self.value = bool(value)
        self._update_light()

    def _update_light(self) -> None:
        match self._light_style:
            case False:
                on = False
            case "always_on":
                on = True
            case "momentary":
                on = self.pressed
            case _:  # "state"
                on = self.value
        self._midi.note_on(self._note, 127 if on else 0)


class MidiRotary(MidiControl):
    """Rotary encoder (continuous CC)."""

    def __init__(self, name: str, config: dict, midi: MidiIO) -> None:
        super().__init__(name, config, midi)
        self._cc = config.get("control", 80)
        self.value = 0

    def on_midi_message(self, msg: "mido.Message") -> dict | None:
        if msg.type == "control_change" and msg.control == self._cc:
            self.value = msg.value
            return {"value": msg.value}
        return None

    def set_value(self, value: Any) -> None:
        if not self.lockout:
            v = max(0, min(127, int(value)))
            self.value = v
            self._midi.control_change(self._cc, v)


class MidiJogWheel(MidiControl):
    """Jog wheel — emits direction +1 or -1."""

    def __init__(self, name: str, config: dict, midi: MidiIO) -> None:
        super().__init__(name, config, midi)
        self._cc = config.get("control", 60)

    def on_midi_message(self, msg: "mido.Message") -> dict | None:
        if msg.type == "control_change" and msg.control == self._cc:
            direction = 1 if msg.value == 65 else -1
            return {"value": direction}
        return None


# Control type registry
_CONTROL_CLASSES: dict[str, type[MidiControl]] = {
    "fader": MidiFader,
    "button": MidiButton,
    "rotary": MidiRotary,
    "jogwheel": MidiJogWheel,
}


# ── LCD Display state ────────────────────────────────────────────────────────

class ScribbleStrip:
    """Behringer X-Touch scribble strip (14 chars, color, invert)."""

    def __init__(self, midi: MidiIO) -> None:
        self._midi = midi
        self.text = " " * 14
        self.color = Color.WHITE
        self.invert = Invert.NONE

    def update(self, text: str | None = None, color: Color | None = None,
               invert: Invert | None = None) -> None:
        if text is not None:
            self.text = (text + " " * 14)[:14]
        if color is not None:
            self.color = color
        if invert is not None:
            self.invert = invert
        self._midi.lcd_update(self.text, self.color, self.invert)

    def update_top(self, text: str, color: Color | None = None) -> None:
        """Update top 7 chars only."""
        text = "{:^7}".format(text[:7])
        self.text = text + self.text[7:14]
        if color is not None:
            self.color = color
        self._midi.lcd_update(self.text, self.color, self.invert)

    def update_bottom(self, text: str, color: Color | None = None) -> None:
        """Update bottom 7 chars only."""
        text = "{:^7}".format(text[:7])
        self.text = self.text[:7] + text
        if color is not None:
            self.color = color
        self._midi.lcd_update(self.text, self.color, self.invert)


# ── MidiSurface: integrates with ozma ControlSurface ─────────────────────────

class MidiSurface(ControlSurface):
    """
    A MIDI device registered as an ozma control surface.

    Config example::

        {
            "device": "X-Touch One",
            "controls": {
                "fader": {"type": "fader", "control": 70, "note": 110,
                          "binding": {"action": "audio.volume", "target": "@active"}},
                "select": {"type": "button", "note": 24, "style": "toggle",
                           "binding": {"action": "scenario.next", "value": 1}},
            },
            "displays": {
                "scribble_top": {"type": "scribble_top",
                                 "binding": "@active.name"},
                "scribble_bottom": {"type": "scribble_bottom",
                                    "binding": "@active.node"},
            }
        }
    """

    def __init__(self, surface_id: str, config: dict) -> None:
        super().__init__(surface_id)
        self._config = config
        self._device_name = config.get("device", "")
        self._midi: MidiIO | None = None
        self._midi_controls: dict[str, MidiControl] = {}
        self._scribble: ScribbleStrip | None = None

        # Message routing: (msg_type, key_value) → MidiControl
        self._msg_map: dict[tuple[str, int], MidiControl] = {}

        # on_control_changed callback set by ControlManager
        self._on_changed: Callable | None = None

        # asyncio event loop ref for thread-safe bridging
        self._loop: asyncio.AbstractEventLoop | None = None

    async def start(self) -> None:
        if not MidiIO.available():
            log.warning("mido not installed — MIDI surface '%s' disabled", self.id)
            return
        try:
            self._loop = asyncio.get_running_loop()
            self._midi = MidiIO(self._device_name, on_message=self._on_midi_raw)
            self._midi.open()
        except Exception as e:
            log.warning("MIDI surface '%s' failed to open: %s", self.id, e)
            self._midi = None
            return

        # Create controls
        for name, cfg in self._config.get("controls", {}).items():
            ctrl_type = cfg.get("type", "button")
            cls = _CONTROL_CLASSES.get(ctrl_type)
            if not cls:
                log.warning("Unknown MIDI control type: %s", ctrl_type)
                continue
            midi_ctrl = cls(name, cfg, self._midi)
            self._midi_controls[name] = midi_ctrl

            # Build message routing map
            if "control" in cfg:
                self._msg_map[("control", cfg["control"])] = midi_ctrl
            if "note" in cfg:
                self._msg_map[("note", cfg["note"])] = midi_ctrl

            # Create ozma Control wrapper with binding
            binding = None
            if "binding" in cfg:
                b = cfg["binding"]
                binding = ControlBinding(
                    action=b.get("action", ""),
                    target=b.get("target", ""),
                    value=b.get("value"),
                    to_target=self._make_transform(b.get("to_target")),
                    from_target=self._make_transform(b.get("from_target")),
                )
            ctrl = Control(name=name, surface_id=self.id, binding=binding)
            ctrl.on_feedback = lambda v, mc=midi_ctrl: mc.set_value(v)
            self.controls[name] = ctrl

        # Create scribble strip
        if self._config.get("displays"):
            self._scribble = ScribbleStrip(self._midi)
            for name, dcfg in self._config["displays"].items():
                display = DisplayControl(
                    name=name, surface_id=self.id,
                    binding=dcfg.get("binding", ""),
                )
                display.on_update = self._make_display_updater(dcfg.get("type", ""))
                self.displays[name] = display

        log.info("MIDI surface '%s' started: %d controls, %d displays",
                 self.id, len(self._midi_controls), len(self.displays))

    async def stop(self) -> None:
        if self._midi:
            self._midi.close()
            log.info("MIDI surface '%s' stopped", self.id)

    def _on_midi_raw(self, msg: "mido.Message") -> None:
        """Called from mido's thread — bridge to async."""
        # Map message type
        type_map = {
            "control_change": ("control", "control"),
            "note_on": ("note", "note"),
            "note_off": ("note", "note"),
        }
        mapped = type_map.get(msg.type)
        if not mapped:
            return
        key = (mapped[0], getattr(msg, mapped[1]))
        midi_ctrl = self._msg_map.get(key)
        if not midi_ctrl:
            return

        delta = midi_ctrl.on_midi_message(msg)
        if delta and "value" in delta and self._loop:
            # Bridge to async event loop
            ctrl = self.controls.get(midi_ctrl.name)
            if ctrl and ctrl.binding and self._on_changed:
                ctrl.value = delta["value"]
                ctrl.lockout = midi_ctrl.lockout
                self._loop.call_soon_threadsafe(
                    asyncio.ensure_future,
                    self._on_changed(self.id, midi_ctrl.name, delta["value"]),
                )

    def _make_display_updater(self, display_type: str) -> Callable:
        """Return a display update function based on type."""
        def update_top(text: str, color: str | None = None) -> None:
            if self._scribble:
                lcd_color = _hex_to_lcd_color(color)
                self._scribble.update_top(text, lcd_color)

        def update_bottom(text: str, color: str | None = None) -> None:
            if self._scribble:
                lcd_color = _hex_to_lcd_color(color)
                self._scribble.update_bottom(text, lcd_color)

        def update_full(text: str, color: str | None = None) -> None:
            if self._scribble:
                lcd_color = _hex_to_lcd_color(color)
                self._scribble.update(text, lcd_color)

        match display_type:
            case "scribble_top":
                return update_top
            case "scribble_bottom":
                return update_bottom
            case "scribble" | _:
                return update_full

    @staticmethod
    def _make_transform(spec: Any) -> Callable | None:
        """
        Build a value transform from a config spec.

        Spec can be:
          - None: no transform
          - "midi_to_float": MIDI 0-127 → float 0.0-1.0
          - "float_to_midi": float 0.0-1.0 → MIDI 0-127
          - dict with "map": scipy interpolation
        """
        if spec is None:
            return None
        if spec == "midi_to_float":
            return lambda v: v / 127.0
        if spec == "float_to_midi":
            return lambda v: int(max(0, min(127, v * 127)))
        if isinstance(spec, dict) and "map" in spec:
            try:
                from scipy.interpolate import interp1d
                x = spec["map"].get("from", [0, 127])
                y = spec["map"].get("to", [0.0, 1.0])
                f = interp1d(x, y, bounds_error=False, fill_value=(y[0], y[-1]))
                return lambda v, _f=f: float(_f(v))
            except ImportError:
                log.warning("scipy not installed — interpolation maps unavailable")
                return None
        return None

    def set_on_changed(self, callback: Callable) -> None:
        """Set the callback for when a control value changes."""
        self._on_changed = callback
