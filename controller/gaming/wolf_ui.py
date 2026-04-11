# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Wolf UI - In-stream app launcher overlay.

Provides an in-stream app launcher accessible via Ctrl+Alt+Shift+W overlay.

Features:
  - In-stream app launcher overlay (Ctrl+Alt+Shift+W)
  - Navigate app list without exiting stream
"""

from __future__ import annotations

import asyncio
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

log = logging.getLogger("ozma.controller.gaming.wolf_ui")


# ─── Constants ───────────────────────────────────────────────────────────────

OVERLAY_HOTKEY = {"key": 35, "modifiers": 0x1C}  # Ctrl+Alt+Shift+W
OVERLAY_ZOOM = 0.8  # 80% of screen
OVERLAY_PADDING = 20
OVERLAY_TIMEOUT = 30  # seconds before auto-hide


# ─── UI Components ───────────────────────────────────────────────────────────

@dataclass
class MenuItem:
    """A menu item in the app launcher."""
    id: str
    label: str
    icon: str = ""  # Base64 icon or emoji
    action: str = ""  # Command/action to execute
    category: str = "games"  # For filtering
    is_game: bool = True
    created_at: float = field(default_factory=time.time)


@dataclass
class OverlayState:
    """State of the overlay."""
    visible: bool = False
    position_x: int = 0
    position_y: int = 0
    items: list[MenuItem] = field(default_factory=list)
    selected_index: int = 0
    category_filter: str = "all"
    search_query: str = ""
    last_activity: float = field(default_factory=time.time)
    created_at: float = field(default_factory=time.time)


# ─── Overlay Manager ─────────────────────────────────────────────────────────

class WolfUIOverlay:
    """
    Manages the Wolf UI overlay for app launching.

    Features:
      - Toggle overlay with hotkey
      - Navigate menu items
      - Execute actions
      - Auto-hide after inactivity
    """

    def __init__(
        self,
        data_dir: Path = Path("/var/lib/ozma/gaming/wolf_ui"),
    ):
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._state = OverlayState()
        self._overlay_visible = False
        self._callbacks: dict[str, Callable] = {}

        # Load persisted state
        self._load_state()

    def _load_state(self) -> None:
        """Load overlay state from disk."""
        state_file = self._data_dir / "overlay_state.json"
        if state_file.exists():
            try:
                import json
                data = json.loads(state_file.read_text())
                self._state.visible = data.get("visible", False)
                self._state.position_x = data.get("position_x", 0)
                self._state.position_y = data.get("position_y", 0)
                self._state.category_filter = data.get("category_filter", "all")
                log.info("Loaded overlay state")
            except Exception as e:
                log.error("Failed to load overlay state: %s", e)

    def _save_state(self) -> None:
        """Save overlay state to disk."""
        state_file = self._data_dir / "overlay_state.json"
        try:
            import json
            data = {
                "visible": self._state.visible,
                "position_x": self._state.position_x,
                "position_y": self._state.position_y,
                "category_filter": self._state.category_filter,
                "last_save": time.time(),
            }
            state_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save overlay state: %s", e)

    def show(self) -> None:
        """Show the overlay."""
        self._state.visible = True
        self._state.last_activity = time.time()
        self._save_state()

        log.info("Overlay shown")
        self._trigger_callback("show")

    def hide(self) -> None:
        """Hide the overlay."""
        self._state.visible = False
        self._state.last_activity = time.time()
        self._save_state()

        log.info("Overlay hidden")
        self._trigger_callback("hide")

    def toggle(self) -> None:
        """Toggle overlay visibility."""
        if self._state.visible:
            self.hide()
        else:
            self.show()

    def is_visible(self) -> bool:
        """Check if overlay is visible."""
        return self._state.visible

    def set_items(self, items: list[MenuItem]) -> None:
        """Set menu items."""
        self._state.items = items
        self._state.selected_index = 0
        self._state.last_activity = time.time()

    def select_next(self) -> None:
        """Select the next menu item."""
        if self._state.items:
            self._state.selected_index = (self._state.selected_index + 1) % len(self._state.items)
            self._state.last_activity = time.time()

    def select_prev(self) -> None:
        """Select the previous menu item."""
        if self._state.items:
            self._state.selected_index = (self._state.selected_index - 1) % len(self._state.items)
            self._state.last_activity = time.time()

    def select_index(self, index: int) -> None:
        """Select a menu item by index."""
        if self._state.items and 0 <= index < len(self._state.items):
            self._state.selected_index = index
            self._state.last_activity = time.time()

    def get_selected_item(self) -> MenuItem | None:
        """Get the currently selected item."""
        if self._state.items:
            return self._state.items[self._state.selected_index]
        return None

    def execute_selected(self) -> bool:
        """Execute the selected item's action."""
        item = self.get_selected_item()
        if not item:
            return False

        self._trigger_callback("execute", item)
        self.hide()
        return True

    def set_filter(self, category: str) -> None:
        """Set the category filter."""
        self._state.category_filter = category
        self._state.last_activity = time.time()

    def set_search(self, query: str) -> None:
        """Set the search query."""
        self._state.search_query = query
        self._state.last_activity = time.time()

    def get_filtered_items(self) -> list[MenuItem]:
        """Get items filtered by current criteria."""
        items = self._state.items

        # Filter by category
        if self._state.category_filter != "all":
            items = [i for i in items if i.category == self._state.category_filter]

        # Filter by search query
        if self._state.search_query:
            query = self._state.search_query.lower()
            items = [i for i in items if query in i.label.lower() or query in i.id.lower()]

        return items

    # ─── Callbacks ────────────────────────────────────────────────────────────

    def on(self, event: str, callback: Callable) -> None:
        """Register a callback for an event."""
        self._callbacks[event] = callback

    def _trigger_callback(self, event: str, data: Any = None) -> None:
        """Trigger a callback."""
        callback = self._callbacks.get(event)
        if callback:
            try:
                if data is not None:
                    callback(data)
                else:
                    callback()
            except Exception as e:
                log.error("Callback error for %s: %s", event, e)

    # ─── Inactivity timeout ───────────────────────────────────────────────────

    def check_timeout(self) -> bool:
        """Check if overlay should auto-hide."""
        if not self._state.visible:
            return False

        elapsed = time.time() - self._state.last_activity
        if elapsed > OVERLAY_TIMEOUT:
            self.hide()
            return True
        return False


# ─── App Launcher ────────────────────────────────────────────────────────────

class AppLauncher:
    """
    Launches applications via Wolf UI.

    Features:
      - App discovery and listing
      - App execution
      - Integration with Moonlight server
    """

    def __init__(
        self,
        moonlight_server: Any = None,
        scenarios: Any = None,
        data_dir: Path = Path("/var/lib/ozma/gaming/wolf_ui"),
    ):
        self._moonlight = moonlight_server
        self._scenarios = scenarios
        self._data_dir = data_dir
        self._data_dir.mkdir(parents=True, exist_ok=True)

        self._overlay = WolfUIOverlay(data_dir)
        self._app_list: list[MenuItem] = []
        self._last_update = 0

        # Load persisted app list
        self._load_app_list()

    def _load_app_list(self) -> None:
        """Load app list from disk."""
        list_file = self._data_dir / "app_list.json"
        if list_file.exists():
            try:
                import json
                data = json.loads(list_file.read_text())
                for app_data in data.get("apps", []):
                    item = MenuItem(**app_data)
                    self._app_list.append(item)
                log.info("Loaded %d apps from list", len(self._app_list))
            except Exception as e:
                log.error("Failed to load app list: %s", e)

    def _save_app_list(self) -> None:
        """Save app list to disk."""
        list_file = self._data_dir / "app_list.json"
        try:
            import json
            data = {
                "apps": [a.to_dict() for a in self._app_list],
                "last_save": time.time(),
            }
            list_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.error("Failed to save app list: %s", e)

    def refresh_app_list(self) -> list[MenuItem]:
        """Refresh the app list from Moonlight server."""
        apps = []

        # Get apps from Moonlight server
        if self._moonlight:
            moonlight_apps = self._moonlight.list_apps()
            for app in moonlight_apps:
                apps.append(MenuItem(
                    id=str(app.app_id),
                    label=app.name,
                    icon=app.icon,
                    category="games" if app.is_game else "apps",
                    is_game=app.is_game,
                ))

        # Get scenarios
        if self._scenarios:
            for scenario in self._scenarios.list_all():
                apps.append(MenuItem(
                    id=scenario.id,
                    label=scenario.name,
                    category="scenarios",
                    is_game=scenario.config.get("is_game", True),
                ))

        # Update app list
        self._app_list = apps
        self._last_update = time.time()
        self._save_app_list()

        log.info("Refreshed app list with %d items", len(apps))
        return apps

    def get_app_list(self) -> list[MenuItem]:
        """Get the current app list."""
        return self._app_list

    def launch_app(self, app_id: str) -> bool:
        """Launch an app by ID."""
        item = next((a for a in self._app_list if a.id == app_id), None)
        if not item:
            return False

        # Execute via Moonlight server
        if self._moonlight:
            try:
                app_id_int = int(app_id)
                return self._moonlight.start_session("wolf-ui-client", app_id_int)
            except (ValueError, TypeError):
                pass

        # Execute via scenario
        if self._scenarios:
            try:
                return self._scenarios.activate(app_id)
            except Exception:
                pass

        log.error("Failed to launch app: %s", app_id)
        return False

    # ─── Overlay integration ──────────────────────────────────────────────────

    def show_overlay(self) -> None:
        """Show the overlay with app list."""
        # Refresh if needed
        if time.time() - self._last_update > 60:  # Every 60 seconds
            self.refresh_app_list()

        self._overlay.set_items(self._app_list)
        self._overlay.show()

    def hide_overlay(self) -> None:
        """Hide the overlay."""
        self._overlay.hide()

    def is_overlay_visible(self) -> bool:
        """Check if overlay is visible."""
        return self._overlay.is_visible()

    # ─── Event handlers ───────────────────────────────────────────────────────

    def on_hotkey(self) -> None:
        """Handle hotkey press."""
        if self._overlay.is_visible():
            self._overlay.hide()
        else:
            self.show_overlay()

    def on_menu_nav(self, direction: int) -> None:
        """Handle menu navigation."""
        if self._overlay.is_visible():
            if direction > 0:
                self._overlay.select_next()
            else:
                self._overlay.select_prev()

    def on_menu_select(self) -> None:
        """Handle menu selection."""
        if self._overlay.is_visible():
            item = self._overlay.get_selected_item()
            if item:
                self.launch_app(item.id)
