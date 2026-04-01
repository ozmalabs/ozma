# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Edge-crossing — mouse cursor flows between machines at screen boundaries.

When the mouse hits the edge of one machine's virtual screen, it crosses
to the adjacent machine.  The cursor appears at the corresponding position
on the other screen.  This makes two (or more) machines feel like one
desktop.

Virtual screen layout:
  Each node has a virtual position + resolution.  The cursor is tracked
  in a global coordinate space spanning all screens.

  Example: SFF PC (1920x1080) on the left, Laptop (1920x1080) on the right:

    [  SFF PC  ][  Laptop  ]
    0     1919  1920   3839

  Mouse at x=1919 (right edge of SFF PC) → moves to x=1920 (left edge
  of Laptop) → HID switches to the laptop node.

Scenario-aware:
  Edge-crossing is enabled/disabled per scenario.  "Work" = enabled
  (mouse flows freely), "Focus" = disabled (cursor stays put),
  "Gaming" = disabled (no accidental screen escape).

Sticky edges:
  Optional: cursor must push against the edge for N ms before crossing.
  Prevents accidental switches when you're just moving the mouse fast.

Resolution mismatch:
  When adjacent screens have different heights, the cursor maps
  proportionally.  If a shorter screen is next to a taller one, the
  cursor hits a "wall" in the unmatched region.
"""

from __future__ import annotations

import logging
import time
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.edge_crossing")


@dataclass
class VirtualScreen:
    """A node's position in the virtual screen layout."""
    node_id: str
    x: int = 0            # Left edge in global coordinates
    y: int = 0            # Top edge in global coordinates
    width: int = 1920
    height: int = 1080

    @property
    def right(self) -> int:
        return self.x + self.width

    @property
    def bottom(self) -> int:
        return self.y + self.height

    def contains(self, gx: int, gy: int) -> bool:
        return self.x <= gx < self.right and self.y <= gy < self.bottom

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id": self.node_id,
            "x": self.x, "y": self.y,
            "width": self.width, "height": self.height,
        }


@dataclass
class EdgeCrossingConfig:
    """Configuration for edge-crossing behaviour."""
    enabled: bool = True
    sticky_ms: float = 0.0        # ms cursor must push edge before crossing (0 = instant)
    wrap: bool = False             # Wrap from last screen back to first


class EdgeCrossingManager:
    """
    Manages the virtual screen layout and detects edge crossings.

    The HIDForwarder calls ``check_crossing()`` on every mouse event.
    If the cursor has crossed an edge, the manager returns the new
    target node and remapped coordinates.
    """

    def __init__(self, state: Any) -> None:
        self._state = state
        self._screens: list[VirtualScreen] = []
        self._config = EdgeCrossingConfig()
        self._current_node: str | None = None
        self._global_x: int = 960     # Current global cursor position
        self._global_y: int = 540
        self._edge_push_start: float = 0.0
        self._edge_direction: str = ""  # "left", "right", "up", "down"

    @property
    def enabled(self) -> bool:
        return self._config.enabled and len(self._screens) > 1

    def set_enabled(self, enabled: bool) -> None:
        self._config.enabled = enabled

    def set_sticky(self, ms: float) -> None:
        self._config.sticky_ms = ms

    def set_layout(self, screens: list[dict]) -> None:
        """Set the virtual screen layout from config."""
        self._screens = [
            VirtualScreen(
                node_id=s["node_id"],
                x=s.get("x", 0), y=s.get("y", 0),
                width=s.get("width", 1920), height=s.get("height", 1080),
            )
            for s in screens
        ]
        # Sort left to right
        self._screens.sort(key=lambda s: s.x)
        if self._screens:
            self._current_node = self._screens[0].node_id
        log.info("Edge-crossing layout: %d screens", len(self._screens))

    def auto_layout(self, node_ids: list[str], width: int = 1920, height: int = 1080) -> None:
        """Auto-arrange nodes left to right."""
        self._screens = []
        x = 0
        for nid in node_ids:
            self._screens.append(VirtualScreen(node_id=nid, x=x, y=0, width=width, height=height))
            x += width
        if self._screens:
            self._current_node = self._screens[0].node_id
        log.info("Edge-crossing auto-layout: %d screens, %dpx total width", len(self._screens), x)

    def get_layout(self) -> list[dict]:
        return [s.to_dict() for s in self._screens]

    def check_crossing(
        self, dx: int, dy: int, current_node_id: str,
    ) -> tuple[str | None, int, int] | None:
        """
        Check if a mouse movement crosses a screen edge.

        Args:
            dx, dy: relative mouse movement (pixels)
            current_node_id: the node currently receiving HID

        Returns:
            None if no crossing, or (new_node_id, new_abs_x, new_abs_y)
            where abs coordinates are in the new screen's 0-32767 range.
        """
        if not self.enabled:
            return None

        # Find current screen
        current = None
        for s in self._screens:
            if s.node_id == current_node_id:
                current = s
                break
        if not current:
            return None

        # Update global position
        self._global_x += dx
        self._global_y += dy

        # Clamp Y to current screen
        self._global_y = max(current.y, min(current.bottom - 1, self._global_y))

        # Check horizontal edges
        if self._global_x < current.x:
            # Crossed left edge
            target = self._find_screen_at(current.x - 1, self._global_y)
            if target and self._check_sticky("left"):
                self._global_x = target.right - 1
                return self._remap(target)
            self._global_x = current.x  # clamp

        elif self._global_x >= current.right:
            # Crossed right edge
            target = self._find_screen_at(current.right, self._global_y)
            if target and self._check_sticky("right"):
                self._global_x = target.x
                return self._remap(target)
            self._global_x = current.right - 1  # clamp

        # Reset edge push if we're not at an edge
        if current.x < self._global_x < current.right - 1:
            self._edge_push_start = 0.0
            self._edge_direction = ""

        return None

    def on_node_switched(self, node_id: str) -> None:
        """Called when the active node changes (from scenario switch, etc.)."""
        self._current_node = node_id
        # Reset global position to centre of the new screen
        for s in self._screens:
            if s.node_id == node_id:
                self._global_x = s.x + s.width // 2
                self._global_y = s.y + s.height // 2
                break

    def _find_screen_at(self, gx: int, gy: int) -> VirtualScreen | None:
        """Find which screen contains the global coordinate."""
        for s in self._screens:
            if s.contains(gx, gy):
                return s
        # Check wrap
        if self._config.wrap and self._screens:
            if gx < self._screens[0].x:
                return self._screens[-1]
            if gx >= self._screens[-1].right:
                return self._screens[0]
        return None

    def _check_sticky(self, direction: str) -> bool:
        """Check if the sticky edge timer has elapsed."""
        if self._config.sticky_ms <= 0:
            return True
        now = time.monotonic()
        if self._edge_direction != direction:
            self._edge_direction = direction
            self._edge_push_start = now
            return False
        elapsed_ms = (now - self._edge_push_start) * 1000
        return elapsed_ms >= self._config.sticky_ms

    def _remap(self, target: VirtualScreen) -> tuple[str, int, int]:
        """Remap global coordinates to the target screen's 0-32767 range."""
        local_x = self._global_x - target.x
        local_y = self._global_y - target.y
        abs_x = int(local_x * 32767 / max(target.width - 1, 1))
        abs_y = int(local_y * 32767 / max(target.height - 1, 1))
        abs_x = max(0, min(32767, abs_x))
        abs_y = max(0, min(32767, abs_y))
        self._current_node = target.node_id
        log.debug("Edge crossing: → %s at (%d, %d)", target.node_id, abs_x, abs_y)
        return (target.node_id, abs_x, abs_y)
