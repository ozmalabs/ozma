# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
OSC (Open Sound Control) surface driver for ozma.

Receives control messages over UDP from any OSC-compatible app (TouchOSC,
Lemur, Open Stage Control, custom ESP32, etc.) and sends state updates back.

Address convention:

  Inbound (app → ozma):
    /ozma/scenario/next          — cycle to next scenario
    /ozma/scenario/prev          — cycle to previous scenario
    /ozma/scenario/activate <id> — activate scenario by ID
    /ozma/volume <float>         — set active node volume (0.0-1.0)
    /ozma/volume/<node> <float>  — set specific node volume
    /ozma/mute                   — toggle mute on active node
    /ozma/mute/<node> <bool>     — set mute on specific node

  Outbound (ozma → app, sent on state change):
    /ozma/state/scenario <id> <name> <color>
    /ozma/state/volume <node> <float>
    /ozma/state/mute <node> <bool>

Default port: 9000 (receive), 9001 (send feedback).
Configure in controls.yaml.

Requires: uv pip install python-osc
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from controls import ControlSurface, Control, ControlBinding

log = logging.getLogger("ozma.osc")

try:
    from pythonosc.dispatcher import Dispatcher
    from pythonosc.osc_server import AsyncIOOSCUDPServer
    from pythonosc.udp_client import SimpleUDPClient
    _OSC_AVAILABLE = True
except ImportError:
    _OSC_AVAILABLE = False


class OSCSurface(ControlSurface):
    """
    An OSC network endpoint registered as an ozma control surface.

    Listens for OSC messages on a UDP port and optionally sends state
    feedback to a configured client address.
    """

    def __init__(
        self,
        surface_id: str = "osc",
        listen_host: str = "0.0.0.0",
        listen_port: int = 9000,
        feedback_host: str | None = None,
        feedback_port: int = 9001,
    ) -> None:
        super().__init__(surface_id)
        self._listen_host = listen_host
        self._listen_port = listen_port
        self._feedback_host = feedback_host
        self._feedback_port = feedback_port
        self._server: Any = None
        self._transport: Any = None
        self._client: Any = None
        self._on_changed: Any = None

        # Build controls for each OSC action
        self.controls["scenario_next"] = Control(
            name="scenario_next", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=1),
        )
        self.controls["scenario_prev"] = Control(
            name="scenario_prev", surface_id=self.id,
            binding=ControlBinding(action="scenario.next", value=-1),
        )
        self.controls["scenario_activate"] = Control(
            name="scenario_activate", surface_id=self.id,
            binding=ControlBinding(action="scenario.activate"),
        )
        self.controls["volume"] = Control(
            name="volume", surface_id=self.id,
            binding=ControlBinding(action="audio.volume", target="@active"),
        )
        self.controls["mute"] = Control(
            name="mute", surface_id=self.id,
            binding=ControlBinding(action="audio.mute", target="@active"),
        )

    async def start(self) -> None:
        if not _OSC_AVAILABLE:
            log.warning("python-osc not installed — OSC surface disabled")
            return

        dispatcher = Dispatcher()
        dispatcher.map("/ozma/scenario/next", self._on_scenario_next)
        dispatcher.map("/ozma/scenario/prev", self._on_scenario_prev)
        dispatcher.map("/ozma/scenario/activate", self._on_scenario_activate)
        dispatcher.map("/ozma/volume", self._on_volume)
        dispatcher.map("/ozma/volume/*", self._on_volume_named)
        dispatcher.map("/ozma/mute", self._on_mute)
        dispatcher.map("/ozma/mute/*", self._on_mute_named)

        try:
            self._server = AsyncIOOSCUDPServer(
                (self._listen_host, self._listen_port),
                dispatcher,
                asyncio.get_running_loop(),
            )
            self._transport, _ = await self._server.create_serve_endpoint()
        except Exception as e:
            log.warning("OSC server failed to start on %s:%d: %s",
                        self._listen_host, self._listen_port, e)
            return

        if self._feedback_host:
            self._client = SimpleUDPClient(self._feedback_host, self._feedback_port)

        log.info("OSC surface listening on %s:%d (feedback → %s:%d)",
                 self._listen_host, self._listen_port,
                 self._feedback_host or "disabled", self._feedback_port)

    async def stop(self) -> None:
        if self._transport:
            self._transport.close()

    def set_on_changed(self, callback: Any) -> None:
        self._on_changed = callback

    # ── Feedback (ozma → OSC client) ─────────────────────────────────────────

    def send_feedback(self, address: str, *args: Any) -> None:
        """Send an OSC message to the feedback client."""
        if self._client:
            try:
                self._client.send_message(address, list(args))
            except Exception:
                pass

    def on_scenario_changed(self, scenario_id: str, name: str, color: str) -> None:
        """Push scenario state to OSC client."""
        self.send_feedback("/ozma/state/scenario", scenario_id, name, color)

    def on_volume_changed(self, node_name: str, volume: float) -> None:
        """Push volume state to OSC client."""
        self.send_feedback("/ozma/state/volume", node_name, volume)

    def on_mute_changed(self, node_name: str, muted: bool) -> None:
        """Push mute state to OSC client."""
        self.send_feedback("/ozma/state/mute", node_name, int(muted))

    # ── OSC message handlers ─────────────────────────────────────────────────

    def _fire(self, control_name: str, value: Any) -> None:
        """Fire a control change (called from OSC dispatcher thread)."""
        if self._on_changed:
            loop = asyncio.get_event_loop()
            loop.call_soon_threadsafe(
                asyncio.ensure_future,
                self._on_changed(self.id, control_name, value),
            )

    def _on_scenario_next(self, address: str, *args: Any) -> None:
        self._fire("scenario_next", 1)

    def _on_scenario_prev(self, address: str, *args: Any) -> None:
        self._fire("scenario_prev", -1)

    def _on_scenario_activate(self, address: str, *args: Any) -> None:
        if args:
            self._fire("scenario_activate", str(args[0]))

    def _on_volume(self, address: str, *args: Any) -> None:
        if args:
            self._fire("volume", float(args[0]))

    def _on_volume_named(self, address: str, *args: Any) -> None:
        """Handle /ozma/volume/<node_name> <float>."""
        if args:
            # Extract node name from address: /ozma/volume/ozma-vm1 → ozma-vm1
            parts = address.split("/")
            if len(parts) >= 4:
                node_name = parts[3]
                # Create a dynamic control for this specific node
                ctrl_name = f"volume_{node_name}"
                if ctrl_name not in self.controls:
                    self.controls[ctrl_name] = Control(
                        name=ctrl_name, surface_id=self.id,
                        binding=ControlBinding(action="audio.volume", target=node_name),
                    )
                self._fire(ctrl_name, float(args[0]))

    def _on_mute(self, address: str, *args: Any) -> None:
        self._fire("mute", True)

    def _on_mute_named(self, address: str, *args: Any) -> None:
        """Handle /ozma/mute/<node_name> [<bool>]."""
        parts = address.split("/")
        if len(parts) >= 4:
            node_name = parts[3]
            ctrl_name = f"mute_{node_name}"
            if ctrl_name not in self.controls:
                self.controls[ctrl_name] = Control(
                    name=ctrl_name, surface_id=self.id,
                    binding=ControlBinding(action="audio.mute", target=node_name),
                )
            mute_val = bool(args[0]) if args else True
            self._fire(ctrl_name, mute_val)
