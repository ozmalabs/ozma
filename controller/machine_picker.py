# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
KVM-level Exposé / Mission Control — hotkey-triggered machine picker.

Press a configurable hotkey (default: ScrollLock+Tab) to show a
thumbnail grid of all machines from their live VNC/capture streams.
Arrow keys or mouse to pick one → scenario switch → overlay dismisses.

This is the multi-machine equivalent of Alt+Tab / Exposé / Mission Control.

Implementation:
  1. Hotkey intercepted in hid.py (before forwarding to active node)
  2. WebSocket event sent to web UI: "picker.show"
  3. Web UI renders live thumbnail grid from existing MJPEG/VNC streams
  4. User selects a machine (click, arrow+enter, or number key)
  5. API activates that scenario
  6. WebSocket event: "picker.hide"

The picker can also be triggered via:
  - API: POST /api/v1/picker/show
  - Stream Deck: dedicated picker key
  - Gamepad: Select + Guide combo
  - OSC: /ozma/picker/show
"""

from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.machine_picker")


@dataclass
class PickerEntry:
    """A machine in the picker grid."""
    scenario_id: str
    name: str
    color: str
    node_id: str
    stream_url: str = ""        # MJPEG or HLS URL for live thumbnail
    is_active: bool = False

    def to_dict(self) -> dict[str, Any]:
        return {
            "scenario_id": self.scenario_id,
            "name": self.name,
            "color": self.color,
            "node_id": self.node_id,
            "stream_url": self.stream_url,
            "is_active": self.is_active,
        }


class MachinePicker:
    """
    Manages the machine picker overlay state.

    The picker itself renders in the web UI (JavaScript). This class
    manages the state and provides the scenario list with stream URLs.
    """

    def __init__(self, state: Any, scenarios: Any, streams: Any = None, captures: Any = None) -> None:
        self._state = state
        self._scenarios = scenarios
        self._streams = streams
        self._captures = captures
        self._visible = False

    @property
    def visible(self) -> bool:
        return self._visible

    async def show(self) -> dict[str, Any]:
        """Show the picker — returns the entry list for the web UI."""
        self._visible = True
        entries = self._build_entries()
        return {"type": "picker.show", "entries": [e.to_dict() for e in entries]}

    async def hide(self) -> dict[str, Any]:
        self._visible = False
        return {"type": "picker.hide"}

    async def select(self, scenario_id: str) -> None:
        """Select a machine from the picker."""
        self._visible = False
        try:
            await self._scenarios.activate(scenario_id)
        except KeyError:
            log.warning("Picker: unknown scenario %s", scenario_id)

    def _build_entries(self) -> list[PickerEntry]:
        """Build the picker entry list from current scenarios."""
        entries = []
        active_id = self._scenarios.active_id

        for sc in self._scenarios.list():
            node_id = sc.get("node_id", "")
            stream_url = ""

            # Find a stream URL for this node
            if self._streams and node_id:
                url = self._streams.stream_url(node_id)
                if url:
                    stream_url = url
                # MJPEG fallback
                if not stream_url:
                    stream_url = f"/api/v1/streams/{node_id}/mjpeg"

            # Check captures
            capture_source = sc.get("capture_source", "")
            if capture_source and not stream_url:
                stream_url = f"/api/v1/captures/{capture_source}/mjpeg"

            entries.append(PickerEntry(
                scenario_id=sc["id"],
                name=sc.get("name", sc["id"]),
                color=sc.get("color", "#888"),
                node_id=node_id,
                stream_url=stream_url,
                is_active=(sc["id"] == active_id),
            ))

        return entries

    def to_dict(self) -> dict[str, Any]:
        return {
            "visible": self._visible,
            "entries": [e.to_dict() for e in self._build_entries()],
        }
