# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Motion device control for ozma.

Controls motorised physical devices — monitor stands, sit/stand desks,
cranes, pan/tilt heads, linear actuators.  Motion devices are another
output type alongside audio and RGB: they have state (position, moving),
accept commands (move, go to preset, stop), and can be bound to control
surfaces (joystick axes, gamepad triggers, MIDI faders).

Architecture:

  Control Surface          ControlManager           Motion Device
  ─────────────────       ─────────────────         ─────────────────
  Joystick X axis   ──→   motion.move              Crane left/right
  Joystick Y axis   ──→   motion.move              Crane up/down
  Gamepad trigger   ──→   motion.move              Monitor stand height
  Scenario switch   ──→   motion.preset            Desk preset "standing"
  Button press      ──→   motion.stop              Emergency stop all

Device types:

  serial   — USB/serial devices (monitor stand, crane, linear actuators)
             Uses a simple text protocol: "MOVE <axis> <speed>\n"
  desk     — Sit/stand desk controllers (BLE or serial)
             Protocol varies by manufacturer; abstracted behind presets
  http     — Network-controlled actuators (ESP32, Raspberry Pi)
             REST API: POST /move, POST /preset, GET /state
  mqtt     — MQTT-controlled devices
             Topic convention: ozma/motion/<device>/<axis>

Motion axes:
  Each device has named axes. Common axes:
    height   — vertical (monitor stand, desk, crane hoist)
    pan      — horizontal rotation (crane swing, pan/tilt)
    tilt     — vertical rotation (pan/tilt head)
    extend   — linear extension (crane boom, trolley)
    x, y, z  — generic cartesian

  Axis values are normalised to -1.0 to +1.0 (velocity) or 0.0 to 1.0
  (position), depending on the control mode.

Control modes:
  velocity — axis value = speed + direction (-1.0 to +1.0). Release = stop.
             Best for joystick control of crane/camera.
  position — axis value = target position (0.0 to 1.0). Device moves there.
             Best for desk height presets, monitor position.

Scenario presets:
  Scenarios can include motion presets:
    {"id": "work", ..., "motion": {"desk": {"height": 0.75}, "monitor": {"height": 0.6}}}
  On scenario switch, all motion devices move to their preset positions.
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.motion")


@dataclass
class MotionAxis:
    """A single axis on a motion device."""

    name: str                  # "height", "pan", "tilt", "extend", "x", "y", "z"
    mode: str = "velocity"     # "velocity" (-1.0 to +1.0) or "position" (0.0 to 1.0)
    value: float = 0.0         # Current value (velocity or position)
    position: float = 0.5      # Current estimated position (0.0-1.0)
    min_pos: float = 0.0       # Position limits
    max_pos: float = 1.0
    speed: float = 1.0         # Movement speed multiplier
    moving: bool = False


@dataclass
class MotionPreset:
    """A named position preset for a device."""

    name: str                  # "standing", "seated", "low", "high"
    axes: dict[str, float]     # axis_name → position (0.0-1.0)


@dataclass
class MotionDevice:
    """A controllable motion device."""

    id: str
    name: str
    device_type: str           # "serial", "desk", "http", "mqtt"
    axes: dict[str, MotionAxis] = field(default_factory=dict)
    presets: dict[str, MotionPreset] = field(default_factory=dict)
    connected: bool = False
    props: dict = field(default_factory=dict)  # Driver-specific config

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "device_type": self.device_type,
            "connected": self.connected,
            "axes": {
                name: {
                    "mode": a.mode,
                    "value": round(a.value, 3),
                    "position": round(a.position, 3),
                    "moving": a.moving,
                }
                for name, a in self.axes.items()
            },
            "presets": {
                name: {"axes": p.axes}
                for name, p in self.presets.items()
            },
        }


# ── Device drivers ───────────────────────────────────────────────────────────

class MotionDriver:
    """Base class for motion device drivers."""

    async def connect(self) -> bool:
        return False

    async def disconnect(self) -> None:
        pass

    async def move_axis(self, axis: str, value: float) -> bool:
        """Set axis velocity (-1.0 to +1.0) or position (0.0 to 1.0)."""
        return False

    async def stop(self, axis: str | None = None) -> bool:
        """Stop movement. axis=None stops all axes."""
        return False

    async def go_to_preset(self, preset: MotionPreset) -> bool:
        """Move to a named preset position."""
        return False

    async def get_position(self, axis: str) -> float | None:
        """Read current position. Returns None if not available."""
        return None


class SerialMotionDriver(MotionDriver):
    """
    Serial/USB motion device driver.

    Protocol (text, newline-terminated):
      Send: MOVE <axis> <speed_float>\n    — set axis velocity
      Send: POS <axis> <position_float>\n  — set axis position
      Send: STOP [axis]\n                  — stop axis or all
      Send: QUERY <axis>\n                 — request position
      Recv: POS <axis> <position_float>\n  — position report
      Recv: OK\n / ERR <msg>\n             — acknowledgement
    """

    def __init__(self, port: str, baudrate: int = 115200) -> None:
        self._port = port
        self._baudrate = baudrate
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None

    async def connect(self) -> bool:
        try:
            import serial_asyncio
            self._reader, self._writer = await serial_asyncio.open_serial_connection(
                url=self._port, baudrate=self._baudrate,
            )
            log.info("Serial motion device connected: %s", self._port)
            return True
        except ImportError:
            log.warning("serial_asyncio not installed — serial motion disabled")
            return False
        except Exception as e:
            log.warning("Serial motion connect failed (%s): %s", self._port, e)
            return False

    async def disconnect(self) -> None:
        if self._writer:
            self._writer.close()

    async def move_axis(self, axis: str, value: float) -> bool:
        return await self._send(f"MOVE {axis} {value:.4f}\n")

    async def stop(self, axis: str | None = None) -> bool:
        cmd = f"STOP {axis}\n" if axis else "STOP\n"
        return await self._send(cmd)

    async def go_to_preset(self, preset: MotionPreset) -> bool:
        for axis, pos in preset.axes.items():
            await self._send(f"POS {axis} {pos:.4f}\n")
        return True

    async def get_position(self, axis: str) -> float | None:
        if await self._send(f"QUERY {axis}\n"):
            return await self._read_position(axis)
        return None

    async def _send(self, cmd: str) -> bool:
        if not self._writer:
            return False
        try:
            self._writer.write(cmd.encode())
            await self._writer.drain()
            return True
        except Exception as e:
            log.debug("Serial send error: %s", e)
            return False

    async def _read_position(self, axis: str, timeout: float = 1.0) -> float | None:
        if not self._reader:
            return None
        try:
            line = await asyncio.wait_for(self._reader.readline(), timeout=timeout)
            parts = line.decode().strip().split()
            if len(parts) >= 3 and parts[0] == "POS" and parts[1] == axis:
                return float(parts[2])
        except Exception:
            pass
        return None


class HTTPMotionDriver(MotionDriver):
    """
    HTTP-controlled motion device (ESP32, Raspberry Pi, etc.).

    API convention:
      POST /move   {"axis": "height", "value": 0.5}
      POST /stop   {"axis": "height"} or {}
      POST /preset {"name": "standing"}
      GET  /state  → {"axes": {"height": {"position": 0.75, "moving": false}}}
    """

    def __init__(self, host: str, port: int = 80) -> None:
        self._host = host
        self._port = port
        self._base_url = f"http://{host}:{port}"

    async def connect(self) -> bool:
        try:
            import urllib.request
            with urllib.request.urlopen(f"{self._base_url}/state", timeout=3):
                return True
        except Exception:
            return False

    async def move_axis(self, axis: str, value: float) -> bool:
        return await self._post("/move", {"axis": axis, "value": value})

    async def stop(self, axis: str | None = None) -> bool:
        body = {"axis": axis} if axis else {}
        return await self._post("/stop", body)

    async def go_to_preset(self, preset: MotionPreset) -> bool:
        return await self._post("/preset", {"name": preset.name, "axes": preset.axes})

    async def get_position(self, axis: str) -> float | None:
        try:
            import urllib.request
            loop = asyncio.get_running_loop()
            def _fetch():
                with urllib.request.urlopen(f"{self._base_url}/state", timeout=3) as r:
                    return json.loads(r.read())
            data = await loop.run_in_executor(None, _fetch)
            return data.get("axes", {}).get(axis, {}).get("position")
        except Exception:
            return None

    async def _post(self, path: str, body: dict) -> bool:
        try:
            import urllib.request
            loop = asyncio.get_running_loop()
            def _do():
                req = urllib.request.Request(
                    f"{self._base_url}{path}",
                    data=json.dumps(body).encode(),
                    headers={"Content-Type": "application/json"},
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=5):
                    pass
            await loop.run_in_executor(None, _do)
            return True
        except Exception as e:
            log.debug("HTTP motion error: %s", e)
            return False


class MQTTMotionDriver(MotionDriver):
    """
    MQTT-controlled motion device.

    Topics:
      ozma/motion/<device_id>/<axis>/set   — publish velocity/position
      ozma/motion/<device_id>/stop         — publish to stop
      ozma/motion/<device_id>/<axis>/state — subscribe for position updates
    """

    def __init__(self, device_id: str, broker: str = "localhost", port: int = 1883) -> None:
        self._device_id = device_id
        self._broker = broker
        self._port = port
        self._client = None

    async def connect(self) -> bool:
        try:
            import paho.mqtt.client as mqtt
            self._client = mqtt.Client()
            self._client.connect(self._broker, self._port, keepalive=60)
            self._client.loop_start()
            return True
        except ImportError:
            log.warning("paho-mqtt not installed — MQTT motion disabled")
            return False
        except Exception as e:
            log.warning("MQTT connect failed: %s", e)
            return False

    async def disconnect(self) -> None:
        if self._client:
            self._client.loop_stop()
            self._client.disconnect()

    async def move_axis(self, axis: str, value: float) -> bool:
        if not self._client:
            return False
        topic = f"ozma/motion/{self._device_id}/{axis}/set"
        self._client.publish(topic, str(value))
        return True

    async def stop(self, axis: str | None = None) -> bool:
        if not self._client:
            return False
        self._client.publish(f"ozma/motion/{self._device_id}/stop", axis or "all")
        return True


# Driver registry
_DRIVER_CLASSES: dict[str, type] = {
    "serial": SerialMotionDriver,
    "http": HTTPMotionDriver,
    "mqtt": MQTTMotionDriver,
}


# ── Motion Manager ───────────────────────────────────────────────────────────

class MotionManager:
    """
    Central manager for all motion devices.

    Usage::

        mgr = MotionManager()
        mgr.add_device(MotionDevice(
            id="crane", name="Workshop Crane", device_type="serial",
            axes={"height": MotionAxis("height", mode="velocity"),
                  "pan": MotionAxis("pan", mode="velocity")},
            props={"port": "/dev/ttyUSB0"},
        ))
        await mgr.start()

        await mgr.move("crane", "pan", 0.5)   # pan right at 50%
        await mgr.move("crane", "pan", 0.0)   # stop panning
        await mgr.go_to_preset("desk", "standing")
    """

    def __init__(self) -> None:
        self._devices: dict[str, MotionDevice] = {}
        self._drivers: dict[str, MotionDriver] = {}
        self._velocity_tasks: dict[str, asyncio.Task] = {}  # device:axis → stop-on-release task

    async def start(self) -> None:
        for device_id, device in self._devices.items():
            driver = self._create_driver(device)
            if driver:
                self._drivers[device_id] = driver
                device.connected = await driver.connect()
                if device.connected:
                    log.info("Motion device connected: %s (%s)", device.name, device.device_type)
                else:
                    log.warning("Motion device failed to connect: %s", device.name)

    async def stop(self) -> None:
        # Stop all movement
        for device_id, driver in self._drivers.items():
            await driver.stop()
            await driver.disconnect()
        for task in self._velocity_tasks.values():
            task.cancel()

    def add_device(self, device: MotionDevice) -> None:
        self._devices[device.id] = device

    def list_devices(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._devices.values()]

    def get_device(self, device_id: str) -> MotionDevice | None:
        return self._devices.get(device_id)

    # ── Movement commands ────────────────────────────────────────────────────

    async def move(self, device_id: str, axis: str, value: float) -> bool:
        """
        Move an axis.

        For velocity mode: value = -1.0 to +1.0 (speed + direction).
                          0.0 = stop. Release joystick = send 0.0.
        For position mode: value = 0.0 to 1.0 (target position).
        """
        device = self._devices.get(device_id)
        driver = self._drivers.get(device_id)
        if not device or not driver or not device.connected:
            return False

        ax = device.axes.get(axis)
        if not ax:
            return False

        ax.value = value
        ax.moving = abs(value) > 0.01

        ok = await driver.move_axis(axis, value)
        return ok

    async def stop_axis(self, device_id: str, axis: str | None = None) -> bool:
        """Stop an axis or all axes on a device."""
        device = self._devices.get(device_id)
        driver = self._drivers.get(device_id)
        if not device or not driver:
            return False

        if axis:
            ax = device.axes.get(axis)
            if ax:
                ax.value = 0.0
                ax.moving = False
        else:
            for ax in device.axes.values():
                ax.value = 0.0
                ax.moving = False

        return await driver.stop(axis)

    async def go_to_preset(self, device_id: str, preset_name: str) -> bool:
        """Move a device to a named preset position."""
        device = self._devices.get(device_id)
        driver = self._drivers.get(device_id)
        if not device or not driver or not device.connected:
            return False

        preset = device.presets.get(preset_name)
        if not preset:
            log.warning("Unknown preset %s on device %s", preset_name, device_id)
            return False

        return await driver.go_to_preset(preset)

    async def on_scenario_switch(self, motion_presets: dict[str, dict[str, float]] | None) -> None:
        """
        Apply scenario motion presets.

        motion_presets: {device_id: {axis: position, ...}, ...}
        e.g. {"desk": {"height": 0.75}, "monitor": {"height": 0.6}}
        """
        if not motion_presets:
            return
        for device_id, axes in motion_presets.items():
            device = self._devices.get(device_id)
            driver = self._drivers.get(device_id)
            if not device or not driver or not device.connected:
                continue
            preset = MotionPreset(name="_scenario", axes=axes)
            await driver.go_to_preset(preset)
            log.info("Motion preset applied: %s → %s", device_id, axes)

    # ── Internals ────────────────────────────────────────────────────────────

    def _create_driver(self, device: MotionDevice) -> MotionDriver | None:
        match device.device_type:
            case "serial":
                return SerialMotionDriver(
                    port=device.props.get("port", "/dev/ttyUSB0"),
                    baudrate=device.props.get("baudrate", 115200),
                )
            case "http":
                return HTTPMotionDriver(
                    host=device.props.get("host", "localhost"),
                    port=device.props.get("port", 80),
                )
            case "mqtt":
                return MQTTMotionDriver(
                    device_id=device.id,
                    broker=device.props.get("broker", "localhost"),
                    port=device.props.get("port", 1883),
                )
            case _:
                log.warning("Unknown motion device type: %s", device.device_type)
                return None
