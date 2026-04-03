# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Ozma Agent system tray — notification area icon with full management UI.

Sits in the Windows notification area (system tray), macOS menu bar, or
Linux indicator area. Provides:

  - Status indicator with colour-coded icon (green/yellow/red)
  - Active scenario display + quick-switch menu
  - Settings dialog (controller URL, machine name, autostart, capture)
  - Connection status + uptime
  - Log viewer (last 100 lines)
  - Open dashboard in browser
  - Start/stop/restart agent
  - System info (hostname, IP, OS, Python version)

The tray runs in the main thread. The agent runs in a background thread.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import socket
import subprocess
import sys
import threading
import time
import webbrowser
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.agent.tray")

# Config file location
if platform.system() == "Windows":
    _CONFIG_DIR = Path(os.environ.get("APPDATA", "~")) / "ozma"
else:
    _CONFIG_DIR = Path.home() / ".config" / "ozma"

_CONFIG_FILE = _CONFIG_DIR / "agent.json"
_LOG_FILE = _CONFIG_DIR / "agent.log"


def _load_config() -> dict:
    try:
        return json.loads(_CONFIG_FILE.read_text())
    except Exception:
        return {}


def _save_config(config: dict) -> None:
    _CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    _CONFIG_FILE.write_text(json.dumps(config, indent=2))


def _get_log_tail(lines: int = 100) -> str:
    try:
        if _LOG_FILE.exists():
            all_lines = _LOG_FILE.read_text(errors="replace").splitlines()
            return "\n".join(all_lines[-lines:])
    except Exception:
        pass
    return "(no logs)"


def _local_ip() -> str:
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 80))
        ip = s.getsockname()[0]
        s.close()
        return ip
    except Exception:
        return "unknown"


class AgentTray:
    """System tray icon for the ozma agent."""

    def __init__(self, agent: Any, controller_url: str = "", name: str = "",
                 agent_api_port: int = 7382) -> None:
        self._agent = agent
        self._controller_url = controller_url
        self._name = name or socket.gethostname()
        self._agent_api_port = agent_api_port
        self._connected = False
        self._active_scenario = ""
        self._scenarios: list[dict] = []
        self._icon = None
        self._start_time = time.time()
        self._config = _load_config()

        # Backup state
        self._backup_health: str = ""         # "green" | "yellow" | "orange" | "red" | "unconfigured"
        self._backup_last_at: str = ""        # ISO timestamp of last successful backup
        self._backup_running: bool = False    # True if a backup is in progress
        self._backup_alert: bool = False      # True when health is orange/red (show badge)

        # Apply saved config
        if not self._controller_url and self._config.get("controller_url"):
            self._controller_url = self._config["controller_url"]
        if self._config.get("name"):
            self._name = self._config["name"]

    def run(self) -> None:
        """Start the tray icon. Blocks the main thread."""
        if platform.system() == "Darwin":
            self._run_macos()
        else:
            self._run_pystray()

    # ── pystray (Windows + Linux) ──────────────────────────────────────

    def _run_pystray(self) -> None:
        try:
            import pystray
            from PIL import Image
        except ImportError:
            log.warning("Tray requires: uv pip install pystray Pillow")
            self._run_headless()
            return

        icon_image = self._create_icon("green")

        self._icon = pystray.Icon(
            "ozma-agent",
            icon_image,
            f"Ozma Agent — {self._name}",
            menu=self._build_menu(),
        )

        # Agent in background thread
        threading.Thread(target=self._run_agent_thread, daemon=True).start()
        # Status polling
        threading.Thread(target=self._poll_status_thread, daemon=True).start()

        self._icon.run()

    def _build_menu(self):
        import pystray

        items = []
        status_colour = "🟢" if self._connected else "🔴"
        uptime = self._format_uptime()

        # ── Header ──
        items.append(pystray.MenuItem(
            f"{status_colour} {'Connected' if self._connected else 'Disconnected'}",
            None, enabled=False,
        ))
        items.append(pystray.MenuItem(f"  {self._name}", None, enabled=False))
        if self._controller_url:
            items.append(pystray.MenuItem(f"  → {self._controller_url}", None, enabled=False))
        items.append(pystray.MenuItem(f"  Uptime: {uptime}", None, enabled=False))

        items.append(pystray.Menu.SEPARATOR)

        # ── Scenarios (quick-switch) ──
        if self._scenarios:
            items.append(pystray.MenuItem("Switch Machine", pystray.Menu(
                *[pystray.MenuItem(
                    f"{'● ' if s.get('id') == self._active_scenario else '  '}{s.get('name', s.get('id', ''))}",
                    self._make_switch(s["id"]),
                    enabled=s.get("id") != self._active_scenario,
                ) for s in self._scenarios]
            )))
            items.append(pystray.Menu.SEPARATOR)

        # ── Actions ──
        items.append(pystray.MenuItem("Open Dashboard", self._on_open_dashboard))
        items.append(pystray.MenuItem("Copy IP Address", self._on_copy_ip))

        # ── Backup status ──
        items.append(pystray.Menu.SEPARATOR)
        backup_icon = self._backup_health_icon()
        backup_label = f"{backup_icon} Backup"
        if self._backup_running:
            backup_label += " (running…)"
        elif self._backup_last_at:
            backup_label += f" — last: {self._backup_last_at}"
        elif self._backup_health == "unconfigured":
            backup_label += " — not configured"
        backup_submenu_items = []
        if self._backup_health:
            health_text = self._backup_health.capitalize() if self._backup_health else "Unknown"
            backup_submenu_items.append(pystray.MenuItem(
                f"Status: {health_text}", None, enabled=False,
            ))
        if self._backup_last_at:
            backup_submenu_items.append(pystray.MenuItem(
                f"Last: {self._backup_last_at}", None, enabled=False,
            ))
        backup_submenu_items.append(pystray.MenuItem("Back Up Now", self._on_backup_now))
        backup_submenu_items.append(pystray.MenuItem(
            "Open Backup Settings",
            lambda _: webbrowser.open(
                (self._controller_url or "http://localhost:7380") + "/#backup"
            ),
        ))
        items.append(pystray.MenuItem(backup_label, pystray.Menu(*backup_submenu_items)))

        items.append(pystray.Menu.SEPARATOR)

        # ── Settings submenu ──
        items.append(pystray.MenuItem("Settings", pystray.Menu(
            pystray.MenuItem(f"Controller: {self._controller_url or '(not set)'}",
                             self._on_set_controller),
            pystray.MenuItem(f"Machine name: {self._name}",
                             self._on_set_name),
            pystray.MenuItem("Autostart on login",
                             self._on_toggle_autostart,
                             checked=lambda _: self._config.get("autostart", False)),
            pystray.MenuItem("Screen capture",
                             self._on_toggle_capture,
                             checked=lambda _: self._config.get("capture", True)),
        )))

        # ── Info submenu ──
        items.append(pystray.MenuItem("System Info", pystray.Menu(
            pystray.MenuItem(f"Host: {socket.gethostname()}", None, enabled=False),
            pystray.MenuItem(f"IP: {_local_ip()}", None, enabled=False),
            pystray.MenuItem(f"OS: {platform.system()} {platform.release()}", None, enabled=False),
            pystray.MenuItem(f"Python: {platform.python_version()}", None, enabled=False),
            pystray.MenuItem(f"Agent: ozma-agent 1.0.0", None, enabled=False),
        )))

        items.append(pystray.MenuItem("View Logs", self._on_view_logs))

        items.append(pystray.Menu.SEPARATOR)

        # ── Quit ──
        items.append(pystray.MenuItem("Restart Agent", self._on_restart))
        items.append(pystray.MenuItem("Quit", self._on_quit))

        return pystray.Menu(*items)

    # ── macOS (rumps) ──────────────────────────────────────────────────

    def _run_macos(self) -> None:
        try:
            import rumps
        except ImportError:
            self._run_pystray()
            return

        tray = self

        class OzmaApp(rumps.App):
            def __init__(self):
                super().__init__("Ozma", quit_button=None)
                self.menu = self._build()

            def _build(self):
                status = "Connected" if tray._connected else "Disconnected"
                return [
                    rumps.MenuItem(f"{status} — {tray._name}"),
                    None,
                    rumps.MenuItem("Open Dashboard", callback=lambda _: webbrowser.open(
                        tray._controller_url or "http://localhost:7380")),
                    None,
                    rumps.MenuItem("Quit", callback=lambda _: rumps.quit_application()),
                ]

        threading.Thread(target=self._run_agent_thread, daemon=True).start()
        OzmaApp().run()

    # ── Headless ──────────────────────────────────────────────────────

    def _run_headless(self) -> None:
        loop = asyncio.new_event_loop()
        try:
            loop.run_until_complete(self._agent.run())
        except KeyboardInterrupt:
            pass
        finally:
            loop.close()

    # ── Agent thread ──────────────────────────────────────────────────

    def _run_agent_thread(self) -> None:
        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(self._agent.run())
        except Exception as e:
            log.error("Agent crashed: %s", e)
        finally:
            loop.close()

    # ── Status polling ────────────────────────────────────────────────

    def _poll_status_thread(self) -> None:
        import urllib.request
        _backup_poll_counter = 0
        while True:
            time.sleep(5)
            if not self._controller_url:
                self._connected = False
                self._update_icon()
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
                self._scenarios = []
                self._active_scenario = ""

            # Poll backup status every 60s (every 12 × 5s iterations)
            _backup_poll_counter += 1
            if _backup_poll_counter >= 12:
                _backup_poll_counter = 0
                self._poll_backup_status()

            self._update_icon()

    def _poll_backup_status(self) -> None:
        """Fetch backup status from the local agent API."""
        import urllib.request
        url = f"http://localhost:{self._agent_api_port}/api/v1/backup/status"
        try:
            with urllib.request.urlopen(url, timeout=3) as r:
                data = json.loads(r.read())
                self._backup_health  = data.get("health", "unconfigured")
                self._backup_running = bool(data.get("running", False))
                last_ts = data.get("last_backup_at") or data.get("last_success_at", "")
                if last_ts:
                    # Format to friendly "YYYY-MM-DD HH:MM"
                    try:
                        import datetime
                        dt = datetime.datetime.fromisoformat(last_ts.replace("Z", "+00:00"))
                        self._backup_last_at = dt.strftime("%Y-%m-%d %H:%M")
                    except Exception:
                        self._backup_last_at = str(last_ts)[:16]
                else:
                    self._backup_last_at = ""
                self._backup_alert = self._backup_health in ("orange", "red")
        except Exception:
            # Agent API not reachable — don't clear existing state
            pass

    def _update_icon(self) -> None:
        if not self._icon:
            return
        try:
            if not self._connected:
                colour = "red"
            elif self._backup_health == "red":
                colour = "red"
            elif self._backup_health in ("orange", "yellow"):
                colour = "orange"
            else:
                colour = "green"
            self._icon.icon = self._create_icon(colour)
            status = "Connected" if self._connected else "Disconnected"
            backup_suffix = ""
            if self._backup_alert:
                backup_suffix = " ⚠ Backup needs attention"
            elif self._backup_health == "unconfigured":
                backup_suffix = " · Backup not configured"
            self._icon.title = f"Ozma Agent — {self._name} ({status}){backup_suffix}"
            self._icon.menu = self._build_menu()
        except Exception:
            pass

    def _backup_health_icon(self) -> str:
        return {
            "green":        "🟢",
            "yellow":       "🟡",
            "orange":       "🟠",
            "red":          "🔴",
            "unconfigured": "⚪",
        }.get(self._backup_health, "⚪")

    # ── Event handlers ────────────────────────────────────────────────

    def _make_switch(self, scenario_id: str):
        def do_switch(_):
            self._switch_scenario(scenario_id)
        return do_switch

    def _switch_scenario(self, scenario_id: str) -> None:
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
            log.debug("Switch failed: %s", e)

    def _on_open_dashboard(self, _) -> None:
        url = self._controller_url or "http://localhost:7380"
        webbrowser.open(url)

    def _on_copy_ip(self, _) -> None:
        ip = _local_ip()
        if platform.system() == "Windows":
            subprocess.run(["clip"], input=ip.encode(), check=False)
        elif platform.system() == "Darwin":
            subprocess.run(["pbcopy"], input=ip.encode(), check=False)
        else:
            try:
                subprocess.run(["xclip", "-selection", "clipboard"],
                               input=ip.encode(), check=False)
            except FileNotFoundError:
                pass

    def _on_set_controller(self, _) -> None:
        url = self._input_dialog("Controller URL",
                                  "Enter the ozma controller URL:",
                                  self._controller_url or "http://localhost:7380")
        if url:
            self._controller_url = url
            self._config["controller_url"] = url
            _save_config(self._config)
            self._update_icon()

    def _on_set_name(self, _) -> None:
        name = self._input_dialog("Machine Name",
                                   "Enter a name for this machine:",
                                   self._name)
        if name:
            self._name = name
            self._config["name"] = name
            _save_config(self._config)
            self._update_icon()

    def _on_toggle_autostart(self, _) -> None:
        current = self._config.get("autostart", False)
        self._config["autostart"] = not current
        _save_config(self._config)
        if not current:
            self._install_autostart()
        else:
            self._remove_autostart()

    def _on_toggle_capture(self, _) -> None:
        current = self._config.get("capture", True)
        self._config["capture"] = not current
        _save_config(self._config)

    def _on_backup_now(self, _) -> None:
        """Trigger an on-demand backup via the local agent API."""
        import urllib.request
        url = f"http://localhost:{self._agent_api_port}/api/v1/backup/run"
        try:
            req = urllib.request.Request(
                url, data=b'{}',
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            urllib.request.urlopen(req, timeout=5)
            self._backup_running = True
            self._update_icon()
        except Exception as e:
            log.debug("Backup now failed: %s", e)

    def _on_view_logs(self, _) -> None:
        logs = _get_log_tail(100)
        if platform.system() == "Windows":
            # Write to temp file and open in notepad
            tmp = Path(os.environ.get("TEMP", "/tmp")) / "ozma-agent-log.txt"
            tmp.write_text(logs)
            subprocess.Popen(["notepad", str(tmp)])
        elif platform.system() == "Darwin":
            tmp = Path("/tmp/ozma-agent-log.txt")
            tmp.write_text(logs)
            subprocess.Popen(["open", "-e", str(tmp)])
        else:
            tmp = Path("/tmp/ozma-agent-log.txt")
            tmp.write_text(logs)
            subprocess.Popen(["xdg-open", str(tmp)])

    def _on_restart(self, _) -> None:
        """Restart the agent process."""
        exe = sys.executable
        args = sys.argv[:]
        if platform.system() == "Windows":
            subprocess.Popen([exe] + args)
        else:
            os.execv(exe, [exe] + args)
        if self._icon:
            self._icon.stop()

    def _on_quit(self, _) -> None:
        if self._icon:
            self._icon.stop()

    # ── Input dialog (platform-specific) ──────────────────────────────

    def _input_dialog(self, title: str, prompt: str, default: str = "") -> str | None:
        if platform.system() == "Windows":
            return self._input_dialog_windows(title, prompt, default)
        else:
            return self._input_dialog_tkinter(title, prompt, default)

    def _input_dialog_windows(self, title: str, prompt: str, default: str) -> str | None:
        """Use PowerShell for a simple input dialog on Windows."""
        try:
            ps = f'''
            Add-Type -AssemblyName Microsoft.VisualBasic
            [Microsoft.VisualBasic.Interaction]::InputBox("{prompt}", "{title}", "{default}")
            '''
            result = subprocess.run(
                ["powershell", "-Command", ps],
                capture_output=True, text=True, timeout=60,
            )
            value = result.stdout.strip()
            return value if value else None
        except Exception:
            return None

    def _input_dialog_tkinter(self, title: str, prompt: str, default: str) -> str | None:
        """Use tkinter for input dialog on Linux/macOS."""
        try:
            import tkinter as tk
            from tkinter import simpledialog
            root = tk.Tk()
            root.withdraw()
            result = simpledialog.askstring(title, prompt, initialvalue=default, parent=root)
            root.destroy()
            return result
        except Exception:
            return None

    # ── Autostart ─────────────────────────────────────────────────────

    def _install_autostart(self) -> None:
        if platform.system() == "Windows":
            # Add to Windows startup via registry
            import winreg
            key = winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                                 r"Software\Microsoft\Windows\CurrentVersion\Run",
                                 0, winreg.KEY_SET_VALUE)
            exe = sys.executable
            cmd = f'"{exe}" -m ozma_desktop_agent --name {self._name}'
            if self._controller_url:
                cmd += f' --controller {self._controller_url}'
            winreg.SetValueEx(key, "OzmaAgent", 0, winreg.REG_SZ, cmd)
            winreg.CloseKey(key)
        elif platform.system() == "Linux":
            # XDG autostart
            autostart_dir = Path.home() / ".config" / "autostart"
            autostart_dir.mkdir(parents=True, exist_ok=True)
            desktop = autostart_dir / "ozma-agent.desktop"
            desktop.write_text(f"""[Desktop Entry]
Type=Application
Name=Ozma Agent
Exec={sys.executable} -m ozma_desktop_agent --name {self._name}
Hidden=false
X-GNOME-Autostart-enabled=true
""")

    def _remove_autostart(self) -> None:
        if platform.system() == "Windows":
            import winreg
            try:
                key = winreg.OpenKey(winreg.HKEY_CURRENT_USER,
                                     r"Software\Microsoft\Windows\CurrentVersion\Run",
                                     0, winreg.KEY_SET_VALUE)
                winreg.DeleteValue(key, "OzmaAgent")
                winreg.CloseKey(key)
            except FileNotFoundError:
                pass
        elif platform.system() == "Linux":
            desktop = Path.home() / ".config" / "autostart" / "ozma-agent.desktop"
            desktop.unlink(missing_ok=True)

    # ── Helpers ───────────────────────────────────────────────────────

    def _format_uptime(self) -> str:
        secs = int(time.time() - self._start_time)
        if secs < 60:
            return f"{secs}s"
        mins = secs // 60
        if mins < 60:
            return f"{mins}m"
        hours = mins // 60
        return f"{hours}h {mins % 60}m"

    def _create_icon(self, colour: str = "green"):
        """Create the tray icon — ozma 'O' with status colour."""
        from PIL import Image, ImageDraw, ImageFont

        size = 64
        img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        draw = ImageDraw.Draw(img)

        # Colour map
        colours = {
            "green":  (74, 224, 164),   # Connected — ozma emerald
            "yellow": (255, 193, 7),    # Connecting / warning
            "orange": (255, 128, 0),    # Backup degraded
            "red":    (220, 53, 69),    # Disconnected / backup failure
        }
        fill = colours.get(colour, colours["green"])

        # Rounded square background
        r = 12
        draw.rounded_rectangle([2, 2, size - 2, size - 2], radius=r, fill=fill)

        # "O" letter in the centre (ozma logo)
        try:
            font = ImageFont.truetype("arial.ttf", 36)
        except Exception:
            try:
                font = ImageFont.truetype("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf", 36)
            except Exception:
                font = ImageFont.load_default()

        draw.text((size // 2, size // 2), "O", fill=(255, 255, 255, 255),
                  font=font, anchor="mm")

        return img
