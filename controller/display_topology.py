# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Physical display topology — spatial awareness of monitors, bezels, and PPI.

Ozma knows what monitors are connected (via EDID), their resolution, physical
size (from EDID), and where they are in space (from user config or auto-detect
via the host agent).  This enables:

  1. **PPI-matched scaling** — content rendered at the correct physical size
     across monitors of different pixel densities.  A 100mm gauge on a 27"
     4K monitor is the same physical size as on a 24" 1080p monitor.

  2. **Bezel compensation** — content that spans monitors accounts for bezel
     width so images align across the gap.  The mouse cursor in edge-crossing
     mode "jumps" the bezel correctly.

  3. **Spatial canvas** — monitors placed on a 2D canvas with real-world
     coordinates (mm from an origin).  Combined with Swordfish mode, this
     enables pixel-accurate multi-monitor content where a line drawn across
     two monitors actually connects.

  4. **3D desk model accuracy** — the Three.js desk scene uses physical
     dimensions to place monitor models at the correct size and position.

Data sources:
  - EDID: physical size in mm (bytes 21-22 of the EDID block)
  - Host agent: display geometry reporting (resolution + arrangement)
  - User config: bezel width, physical position (for fine-tuning)
  - Capture card: current signal resolution

From these, we derive:
  - PPI (pixels per inch) per monitor
  - mm-per-pixel per monitor
  - Total canvas in mm (for Swordfish and wall modes)
  - Bezel-compensated edge-crossing coordinates
"""

from __future__ import annotations

import logging
import math
from dataclasses import dataclass, field
from typing import Any

log = logging.getLogger("ozma.display_topology")


@dataclass
class PhysicalDisplay:
    """A physical monitor with spatial properties."""

    id: str                         # Unique ID (capture source, node, or agent display)
    name: str = ""

    # Pixel dimensions (from V4L2, EDID, or agent)
    width_px: int = 1920
    height_px: int = 1080

    # Physical dimensions (from EDID bytes 21-22, in mm)
    width_mm: float = 0.0           # 0 = unknown (will estimate from diagonal)
    height_mm: float = 0.0

    # Diagonal (user-provided, in inches — e.g., 27, 48)
    diagonal_inches: float = 0.0

    # Bezel (user-configurable, mm)
    bezel_top: float = 5.0
    bezel_bottom: float = 5.0
    bezel_left: float = 5.0
    bezel_right: float = 5.0

    # Spatial position on the physical canvas (mm from origin)
    pos_x_mm: float = 0.0
    pos_y_mm: float = 0.0
    rotation_deg: float = 0.0       # Physical rotation (portrait = 90)

    # Orientation
    orientation: str = "landscape"   # landscape, portrait

    # Source
    source_type: str = ""           # "capture", "agent", "vnc", "dbus", "ivshmem", "manual"
    node_id: str = ""
    display_index: int = 0          # Which display head on this node (for multi-monitor)

    def __post_init__(self) -> None:
        self._compute_physical_size()

    def _compute_physical_size(self) -> None:
        """Compute physical dimensions from available data."""
        if self.width_mm > 0 and self.height_mm > 0:
            return  # Already have physical dimensions

        if self.diagonal_inches > 0:
            # Compute from diagonal + aspect ratio
            aspect = self.width_px / max(self.height_px, 1)
            diag_mm = self.diagonal_inches * 25.4
            self.height_mm = diag_mm / math.sqrt(1 + aspect ** 2)
            self.width_mm = self.height_mm * aspect
            return

        # Default estimate: assume ~96 DPI
        self.width_mm = self.width_px * 25.4 / 96
        self.height_mm = self.height_px * 25.4 / 96

    @property
    def ppi(self) -> float:
        """Pixels per inch (horizontal)."""
        if self.width_mm <= 0:
            return 96.0
        return self.width_px / (self.width_mm / 25.4)

    @property
    def mm_per_pixel(self) -> float:
        """Millimetres per pixel (horizontal)."""
        if self.width_px <= 0:
            return 0.265  # ~96 DPI
        return self.width_mm / self.width_px

    @property
    def total_width_mm(self) -> float:
        """Total width including bezels."""
        return self.width_mm + self.bezel_left + self.bezel_right

    @property
    def total_height_mm(self) -> float:
        """Total height including bezels."""
        return self.height_mm + self.bezel_top + self.bezel_bottom

    @property
    def active_rect_mm(self) -> tuple[float, float, float, float]:
        """Active display area in canvas mm: (left, top, right, bottom)."""
        left = self.pos_x_mm + self.bezel_left
        top = self.pos_y_mm + self.bezel_top
        return (left, top, left + self.width_mm, top + self.height_mm)

    def px_to_mm(self, px_x: int, px_y: int) -> tuple[float, float]:
        """Convert pixel coordinates to canvas mm coordinates."""
        mpp = self.mm_per_pixel
        left, top, _, _ = self.active_rect_mm
        return (left + px_x * mpp, top + px_y * mpp)

    def mm_to_px(self, mm_x: float, mm_y: float) -> tuple[int, int]:
        """Convert canvas mm coordinates to pixel coordinates."""
        mpp = self.mm_per_pixel
        left, top, _, _ = self.active_rect_mm
        return (int((mm_x - left) / mpp), int((mm_y - top) / mpp))

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id, "name": self.name,
            "width_px": self.width_px, "height_px": self.height_px,
            "width_mm": round(self.width_mm, 1), "height_mm": round(self.height_mm, 1),
            "diagonal_inches": self.diagonal_inches,
            "ppi": round(self.ppi, 1),
            "mm_per_pixel": round(self.mm_per_pixel, 4),
            "bezel": {
                "top": self.bezel_top, "bottom": self.bezel_bottom,
                "left": self.bezel_left, "right": self.bezel_right,
            },
            "position_mm": {"x": self.pos_x_mm, "y": self.pos_y_mm},
            "rotation": self.rotation_deg,
            "orientation": self.orientation,
            "total_size_mm": {
                "width": round(self.total_width_mm, 1),
                "height": round(self.total_height_mm, 1),
            },
        }

    @classmethod
    def from_edid(cls, display_id: str, edid_data: bytes, **kwargs: Any) -> "PhysicalDisplay":
        """Create from EDID binary data (128+ bytes)."""
        w_mm = 0.0
        h_mm = 0.0
        if len(edid_data) >= 22:
            w_mm = float(edid_data[21]) * 10  # EDID stores cm, convert to mm
            h_mm = float(edid_data[22]) * 10
        return cls(id=display_id, width_mm=w_mm, height_mm=h_mm, **kwargs)


class DisplayTopology:
    """
    Manages the spatial layout of all physical displays.

    Provides:
      - PPI-matched scaling factors between any two displays
      - Bezel-compensated coordinates for edge-crossing
      - Canvas bounds for Swordfish mode
      - Physical layout data for the 3D desk model
    """

    def __init__(self) -> None:
        self._displays: dict[str, PhysicalDisplay] = {}

    def add_display(self, display: PhysicalDisplay) -> None:
        self._displays[display.id] = display
        log.info("Display topology: %s (%dx%d px, %.0fx%.0f mm, %.0f PPI)",
                 display.name or display.id,
                 display.width_px, display.height_px,
                 display.width_mm, display.height_mm, display.ppi)

    def remove_display(self, display_id: str) -> None:
        self._displays.pop(display_id, None)

    def get_display(self, display_id: str) -> PhysicalDisplay | None:
        return self._displays.get(display_id)

    def list_displays(self) -> list[dict[str, Any]]:
        return [d.to_dict() for d in self._displays.values()]

    def auto_arrange_horizontal(self, display_ids: list[str] | None = None) -> None:
        """Auto-arrange displays left to right with bezel gaps."""
        displays = [self._displays[did] for did in (display_ids or self._displays.keys())
                     if did in self._displays]
        x = 0.0
        for d in displays:
            d.pos_x_mm = x
            d.pos_y_mm = 0.0
            x += d.total_width_mm

    # ── Scaling ──────────────────────────────────────────────────────────────

    def scale_factor(self, from_id: str, to_id: str) -> float:
        """
        Compute the scale factor to make content appear the same physical
        size on two different displays.

        E.g., a 100px gauge on a 110 PPI monitor needs to be 183px on a
        184 PPI monitor to appear the same physical size.
        """
        from_d = self._displays.get(from_id)
        to_d = self._displays.get(to_id)
        if not from_d or not to_d:
            return 1.0
        return to_d.ppi / from_d.ppi

    def physical_size_px(self, display_id: str, mm: float) -> int:
        """Convert a physical size (mm) to pixels on a specific display."""
        d = self._displays.get(display_id)
        if not d:
            return int(mm * 96 / 25.4)
        return int(mm / d.mm_per_pixel)

    # ── Edge-crossing with bezel compensation ────────────────────────────────

    def bezel_gap_px(self, left_id: str, right_id: str) -> int:
        """
        Compute the pixel-equivalent gap between two adjacent displays,
        accounting for bezels.  Used by edge-crossing to "jump" the cursor
        past the bezel gap.
        """
        left = self._displays.get(left_id)
        right = self._displays.get(right_id)
        if not left or not right:
            return 0

        gap_mm = left.bezel_right + right.bezel_left
        # Use the average mm_per_pixel of both displays
        avg_mpp = (left.mm_per_pixel + right.mm_per_pixel) / 2
        return int(gap_mm / avg_mpp)

    # ── Canvas bounds ────────────────────────────────────────────────────────

    def canvas_bounds_mm(self) -> tuple[float, float, float, float]:
        """Return (min_x, min_y, max_x, max_y) of all displays in mm."""
        if not self._displays:
            return (0, 0, 0, 0)
        min_x = min(d.pos_x_mm for d in self._displays.values())
        min_y = min(d.pos_y_mm for d in self._displays.values())
        max_x = max(d.pos_x_mm + d.total_width_mm for d in self._displays.values())
        max_y = max(d.pos_y_mm + d.total_height_mm for d in self._displays.values())
        return (min_x, min_y, max_x, max_y)

    def canvas_size_mm(self) -> tuple[float, float]:
        """Total canvas size in mm."""
        x1, y1, x2, y2 = self.canvas_bounds_mm()
        return (x2 - x1, y2 - y1)
