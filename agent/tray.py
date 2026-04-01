# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Agent system tray icon — cross-platform GUI management.

Sits in the system tray (Windows/macOS) or indicator area (Linux).
Shows agent status, active machine, quick-switch menu, and settings.

Features:
  - Status indicator: green (connected), yellow (connecting), red (offline)
  - Active machine name shown in tooltip
  - Quick-switch: right-click → list of scenarios → click to switch
  - Volume slider in the menu (if supported by the platform)
  - Open dashboard in browser
  - Settings: controller URL, machine name, autostart
  - Logs viewer (last 50 lines)
  - Quit (stops the agent but doesn't uninstall the service)

Cross-platform:
  Windows:  pystray (pip install pystray Pillow)
  macOS:    rumps (pip install rumps) — native menu bar
  Linux:    pystray with appindicator backend, or falls back to no tray

The tray runs in the main thread. The agent runs in a background thread.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import sys
import threading
import time
import webbrowser
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.agent.tray")


class AgentTray:
    """
    System tray icon for the ozma agent.

    Manages the agent lifecycle and provides a GUI for quick actions.
    """

    def __init__(self, agent: Any, controller_url: str = "", name: str = "") -> None:
        self._agent = agent
        self._controller_url = controller_url
        self._name = name
        self._connected = False
        self._active_scenario = ""
        self._scenarios: list[dict] = []
        self._icon = None
        self._poll_task: asyncio.Task | None = None

    def run(self) -> None:
        """Start the tray icon. Blocks the main thread."""
        system = platform.system()
        if system == "Darwin":
            self._run_macos()
        else:
            self._run_pystray()

    # ── pystray (Windows + Linux) ──────────────────────────────────────

    def _run_pystray(self) -> None:
        try:
            import pystray
            from PIL import Image, ImageDraw
        except ImportError:
            log.warning("Tray icon requires: pip install pystray Pillow")
            log.info("Running agent without tray icon")
            self._run_headless()
            return

        # Create the icon image
        icon_image = self._create_icon_image()

        def on_open_dashboard(_):
            url = self._controller_url or "http://localhost:7380"
            webbrowser.open(url)

        def on_quit(_):
            if self._icon:
                self._icon.stop()

        def build_menu():
            import pystray
            items = []

            # Status
            status = "Connected" if self._connected else "Disconnected"
            items.append(pystray.MenuItem(f"Status: {status}", None, enabled=False))
            items.append(pystray.MenuItem(f"Machine: {self._name}", None, enabled=False))

            if self._active_scenario:
                items.append(pystray.MenuItem(f"Active: {self._active_scenario}", None, enabled=False))

            items.append(pystray.Menu.SEPARATOR)

            # Scenarios (quick-switch)
            if self._scenarios:
                for s in self._scenarios:
                    scenario_name = s.get("name", s.get("id", ""))
                    is_active = s.get("id") == self._active_scenario

                    def make_switch(sid):
                        def do_switch(_):
                            self._switch_scenario(sid)
                        return do_switch

                    items.append(pystray.MenuItem(
                        f"{'● ' if is_active else '  '}{scenario_name}",
                        make_switch(s["id"]),
                        enabled=not is_active,
                    ))
                items.append(pystray.Menu.SEPARATOR)

            # Actions
            items.append(pystray.MenuItem("Open Dashboard", on_open_dashboard))
            items.append(pystray.Menu.SEPARATOR)
            items.append(pystray.MenuItem("Quit", on_quit))

            return pystray.Menu(*items)

        self._icon = pystray.Icon(
            "ozma-agent",
            icon_image,
            f"Ozma Agent — {self._name}",
            menu=build_menu(),
        )

        # Start agent in background thread
        agent_thread = threading.Thread(target=self._run_agent_thread, daemon=True)
        agent_thread.start()

        # Start polling for status updates
        poll_thread = threading.Thread(target=self._poll_status_thread, daemon=True)
        poll_thread.start()

        # Run tray icon (blocks main thread)
        self._icon.run()

    # ── rumps (macOS) ──────────────────────────────────────────────────

    def _run_macos(self) -> None:
        try:
            import rumps
        except ImportError:
            log.warning("macOS tray icon requires: pip install rumps")
            self._run_pystray()
            return

        agent = self._agent
        controller_url = self._controller_url
        name = self._name
        tray = self

        class OzmaAgentApp(rumps.App):
            def __init__(self):
                super().__init__("Ozma", icon=None, quit_button=None)
                self.menu = [
                    rumps.MenuItem(f"Machine: {name}", callback=None),
                    rumps.MenuItem("Status: Connecting...", callback=None),
                    None,  # separator
                    rumps.MenuItem("Open Dashboard", callback=self.open_dashboard),
                    None,
                    rumps.MenuItem("Quit", callback=self.quit_app),
                ]

            @rumps.clicked("Open Dashboard")
            def open_dashboard(self, _):
                url = controller_url or "http://localhost:7380"
                webbrowser.open(url)

            @rumps.clicked("Quit")
            def quit_app(self, _):
                rumps.quit_application()

        # Start agent in background
        agent_thread = threading.Thread(target=self._run_agent_thread, daemon=True)
        agent_thread.start()

        app = OzmaAgentApp()
        app.run()

    # ── Headless fallback ──────────────────────────────────────────────

    def _run_headless(self) -> None:
        """No tray icon — just run the agent directly."""
        loop = asyncio.new_event_loop()
        try:
            loop.run_until_complete(self._agent.run())
        except KeyboardInterrupt:
            pass
        finally:
            loop.close()

    # ── Agent thread ───────────────────────────────────────────────────

    def _run_agent_thread(self) -> None:
        """Run the agent in a background thread with its own event loop."""
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(self._agent.run())
        except Exception as e:
            log.error("Agent crashed: %s", e)
        finally:
            loop.close()

    # ── Status polling ─────────────────────────────────────────────────

    def _poll_status_thread(self) -> None:
        """Poll the controller for status updates to refresh the menu."""
        import urllib.request
        while True:
            time.sleep(5)
            if not self._controller_url:
                continue
            try:
                url = f"{self._controller_url.rstrip('/')}/api/v1/scenarios"
                with urllib.request.urlopen(url, timeout=3) as r:
                    data = json.loads(r.read())
                    self._scenarios = data.get("scenarios", [])
                    self._active_scenario = data.get("active", "")
                    self._connected = True
            except Exception:
                self._connected = False

            # Update tray icon tooltip
            if self._icon:
                status = "Connected" if self._connected else "Disconnected"
                self._icon.title = f"Ozma Agent — {self._name} ({status})"
                # Rebuild menu with updated scenarios
                try:
                    self._icon.menu = self._build_pystray_menu()
                except Exception:
                    pass

    def _build_pystray_menu(self):
        """Rebuild the pystray menu with current state."""
        import pystray
        items = []
        status = "Connected" if self._connected else "Disconnected"
        items.append(pystray.MenuItem(f"Status: {status}", None, enabled=False))
        items.append(pystray.MenuItem(f"Machine: {self._name}", None, enabled=False))
        if self._active_scenario:
            items.append(pystray.MenuItem(f"Active: {self._active_scenario}", None, enabled=False))
        items.append(pystray.Menu.SEPARATOR)
        if self._scenarios:
            for s in self._scenarios:
                sname = s.get("name", s.get("id", ""))
                is_active = s.get("id") == self._active_scenario
                def make_switch(sid):
                    def do_switch(_): self._switch_scenario(sid)
                    return do_switch
                items.append(pystray.MenuItem(
                    f"{'● ' if is_active else '  '}{sname}",
                    make_switch(s["id"]), enabled=not is_active,
                ))
            items.append(pystray.Menu.SEPARATOR)
        items.append(pystray.MenuItem("Open Dashboard", lambda _: webbrowser.open(self._controller_url or "http://localhost:7380")))
        items.append(pystray.Menu.SEPARATOR)
        items.append(pystray.MenuItem("Quit", lambda _: self._icon.stop() if self._icon else None))
        return pystray.Menu(*items)

    # ── Actions ────────────────────────────────────────────────────────

    def _switch_scenario(self, scenario_id: str) -> None:
        """Switch scenario via the controller API."""
        import urllib.request
        if not self._controller_url:
            return
        try:
            url = f"{self._controller_url.rstrip('/')}/api/v1/scenarios/{scenario_id}/activate"
            req = urllib.request.Request(url, data=b'{}',
                                         headers={"Content-Type": "application/json"},
                                         method="POST")
            urllib.request.urlopen(req, timeout=5)
        except Exception as e:
            log.debug("Scenario switch failed: %s", e)

    # ── Icon image ─────────────────────────────────────────────────────

    def _create_icon_image(self):
        """Create a simple tray icon image (green circle on transparent)."""
        from PIL import Image, ImageDraw
        size = 64
        img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        draw = ImageDraw.Draw(img)
        # Emerald circle (ozma brand colour)
        draw.ellipse([8, 8, size - 8, size - 8], fill=(74, 224, 164, 255))
        # "O" in the centre
        try:
            draw.text((size // 2 - 6, size // 2 - 8), "O", fill=(0, 0, 0, 255))
        except Exception:
            pass
        return img
