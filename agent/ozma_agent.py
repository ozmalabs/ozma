# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
#!/usr/bin/env python3
"""
Ozma Host Agent — optional software running on target machines.

Enhances the KVM experience but is never required.  Machines work
perfectly without it — the agent is purely additive.

Features:
  1. Clipboard sync — watch system clipboard, send to controller on copy.
     On scenario switch, controller pushes shared clipboard to this machine.
  2. Display geometry — report screen resolution + arrangement to controller.
     Enables auto-configuration of virtual screen layout for edge-crossing.
  3. Resolution change — change display resolution on command from controller
     (paired with EDID override for capture cards).

Discovery:
  Auto-discovers the controller via mDNS (_ozma-desk._tcp) on the
  isolated KVM network.  Falls back to manual URL configuration.

Communication:
  Connects to the controller's REST API + WebSocket for real-time events.
  Optionally uses the USB HID vendor-specific channel for clipboard
  (avoids needing network access on the host — just USB).

Cross-platform: Linux (xclip/xsel/wl-copy), macOS (pbcopy/pbpaste),
Windows (pyperclip).  Distributed as AppImage, DMG, portable EXE.

Usage:
  python ozma_agent.py [--controller http://10.0.100.1:7380]
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path

log = logging.getLogger("ozma.agent")

CONTROLLER_PORT = 7380
POLL_INTERVAL = 0.5  # clipboard check interval


class ClipboardManager:
    """Cross-platform clipboard read/write."""

    def __init__(self) -> None:
        self._system = platform.system()
        self._last_content = ""

    def read(self) -> str:
        """Read current clipboard content."""
        try:
            if self._system == "Darwin":
                return subprocess.run(["pbpaste"], capture_output=True, text=True, timeout=2).stdout
            elif self._system == "Linux":
                for cmd in [["wl-paste", "--no-newline"], ["xclip", "-selection", "clipboard", "-o"], ["xsel", "-b", "-o"]]:
                    if shutil.which(cmd[0]):
                        return subprocess.run(cmd, capture_output=True, text=True, timeout=2).stdout
            elif self._system == "Windows":
                try:
                    import pyperclip
                    return pyperclip.paste()
                except ImportError:
                    return subprocess.run(["powershell", "-command", "Get-Clipboard"], capture_output=True, text=True, timeout=2).stdout
        except Exception:
            pass
        return ""

    def write(self, content: str) -> None:
        """Write content to clipboard."""
        try:
            if self._system == "Darwin":
                subprocess.run(["pbcopy"], input=content.encode(), timeout=2)
            elif self._system == "Linux":
                for cmd in [["wl-copy"], ["xclip", "-selection", "clipboard"], ["xsel", "-b", "-i"]]:
                    if shutil.which(cmd[0]):
                        subprocess.run(cmd, input=content.encode(), timeout=2)
                        return
            elif self._system == "Windows":
                try:
                    import pyperclip
                    pyperclip.copy(content)
                except ImportError:
                    subprocess.run(["clip"], input=content.encode(), timeout=2)
        except Exception:
            pass

    def has_changed(self) -> str | None:
        """Check if clipboard changed since last check. Returns new content or None."""
        current = self.read()
        if current and current != self._last_content:
            self._last_content = current
            return current
        return None


class DisplayInfo:
    """Cross-platform display geometry reporter."""

    @staticmethod
    def get_screens() -> list[dict]:
        """Return list of screens with position and resolution."""
        system = platform.system()
        screens = []

        if system == "Linux":
            screens = DisplayInfo._linux_screens()
        elif system == "Darwin":
            screens = DisplayInfo._macos_screens()
        elif system == "Windows":
            screens = DisplayInfo._windows_screens()

        return screens or [{"x": 0, "y": 0, "width": 1920, "height": 1080, "primary": True}]

    @staticmethod
    def _linux_screens() -> list[dict]:
        try:
            result = subprocess.run(["xrandr", "--query"], capture_output=True, text=True, timeout=5)
            import re
            screens = []
            for line in result.stdout.splitlines():
                m = re.match(r"(\S+)\s+connected\s+(?:primary\s+)?(\d+)x(\d+)\+(\d+)\+(\d+)", line)
                if m:
                    screens.append({
                        "name": m.group(1),
                        "width": int(m.group(2)), "height": int(m.group(3)),
                        "x": int(m.group(4)), "y": int(m.group(5)),
                        "primary": "primary" in line,
                    })
            return screens
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return []

    @staticmethod
    def _macos_screens() -> list[dict]:
        try:
            result = subprocess.run(
                ["system_profiler", "SPDisplaysDataType", "-json"],
                capture_output=True, text=True, timeout=10,
            )
            data = json.loads(result.stdout)
            screens = []
            for gpu in data.get("SPDisplaysDataType", []):
                for disp in gpu.get("spdisplays_ndrvs", []):
                    res = disp.get("_spdisplays_resolution", "")
                    import re
                    m = re.match(r"(\d+)\s*x\s*(\d+)", res)
                    if m:
                        screens.append({
                            "width": int(m.group(1)), "height": int(m.group(2)),
                            "x": 0, "y": 0,
                            "primary": disp.get("spdisplays_main") == "spdisplays_yes",
                        })
            return screens
        except Exception:
            return []

    @staticmethod
    def _windows_screens() -> list[dict]:
        try:
            result = subprocess.run(
                ["powershell", "-command",
                 "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Screen]::AllScreens | ForEach-Object { $_.Bounds.X, $_.Bounds.Y, $_.Bounds.Width, $_.Bounds.Height, $_.Primary }"],
                capture_output=True, text=True, timeout=10,
            )
            lines = result.stdout.strip().split("\n")
            screens = []
            for i in range(0, len(lines) - 4, 5):
                screens.append({
                    "x": int(lines[i]), "y": int(lines[i+1]),
                    "width": int(lines[i+2]), "height": int(lines[i+3]),
                    "primary": lines[i+4].strip().lower() == "true",
                })
            return screens
        except Exception:
            return []

    @staticmethod
    def set_resolution(width: int, height: int, display: str = "") -> bool:
        """Change display resolution."""
        system = platform.system()
        try:
            if system == "Linux":
                cmd = ["xrandr", "--output", display or "eDP-1", "--mode", f"{width}x{height}"]
                return subprocess.run(cmd, timeout=5).returncode == 0
            elif system == "Darwin":
                # Requires displayplacer or similar
                return False
            elif system == "Windows":
                # Use PowerShell + ChangeDisplaySettingsEx
                return False
        except Exception:
            return False


class WallpaperManager:
    """
    Cross-platform desktop wallpaper management.

    Changes the OS desktop background when a scenario switches.
    Each scenario can specify a wallpaper: a URL, a local path, a solid
    colour, or a colour generated from the scenario colour.

    Wallpaper modes:
      image     — set a specific image file as wallpaper
      color     — set a solid colour (from scenario colour)
      gradient  — generate a gradient from scenario colour (dark → colour → dark)
      url       — download an image from a URL and set it
      restore   — restore the original wallpaper (before ozma changed it)
    """

    def __init__(self) -> None:
        self._system = platform.system()
        self._original_wallpaper: str = ""
        self._temp_dir = Path("/tmp/ozma-wallpapers") if self._system != "Windows" else Path(os.environ.get("TEMP", "/tmp")) / "ozma-wallpapers"
        self._temp_dir.mkdir(parents=True, exist_ok=True)

    def save_original(self) -> None:
        """Save the current wallpaper so we can restore it later."""
        self._original_wallpaper = self._get_current() or ""

    def restore_original(self) -> None:
        """Restore the wallpaper that was set before ozma changed it."""
        if self._original_wallpaper:
            self.set_image(self._original_wallpaper)

    def set_image(self, path: str) -> bool:
        """Set a local image file as the desktop wallpaper."""
        try:
            if self._system == "Linux":
                return self._linux_set(path)
            elif self._system == "Darwin":
                return self._macos_set(path)
            elif self._system == "Windows":
                return self._windows_set(path)
        except Exception as e:
            log.debug("Wallpaper set failed: %s", e)
        return False

    def set_color(self, hex_color: str) -> bool:
        """Generate a solid-colour wallpaper and set it."""
        try:
            from PIL import Image
            img = Image.new("RGB", (1920, 1080), self._hex_to_rgb(hex_color))
            path = str(self._temp_dir / "solid.png")
            img.save(path)
            return self.set_image(path)
        except ImportError:
            # Fallback: write a tiny BMP manually
            return self._set_color_fallback(hex_color)

    def set_gradient(self, hex_color: str) -> bool:
        """Generate a vertical gradient wallpaper: dark → colour → dark."""
        try:
            from PIL import Image, ImageDraw
            r, g, b = self._hex_to_rgb(hex_color)
            img = Image.new("RGB", (1920, 1080))
            draw = ImageDraw.Draw(img)
            for y in range(1080):
                # Sine curve: dark at top/bottom, colour in middle
                import math
                t = math.sin(y / 1080.0 * math.pi) * 0.6 + 0.1
                draw.line([(0, y), (1920, y)],
                          fill=(int(r * t), int(g * t), int(b * t)))
            path = str(self._temp_dir / "gradient.png")
            img.save(path)
            return self.set_image(path)
        except ImportError:
            return self.set_color(hex_color)

    def set_from_url(self, url: str) -> bool:
        """Download an image and set it as wallpaper."""
        try:
            import urllib.request
            path = str(self._temp_dir / "downloaded.png")
            urllib.request.urlretrieve(url, path)
            return self.set_image(path)
        except Exception:
            return False

    def apply_scenario(self, scenario: dict) -> bool:
        """
        Apply wallpaper settings from a scenario.

        Scenario wallpaper config:
          {"wallpaper": {"mode": "gradient", "color": "#4A90D9"}}
          {"wallpaper": {"mode": "image", "path": "/path/to/image.jpg"}}
          {"wallpaper": {"mode": "url", "url": "https://..."}}
          {"wallpaper": {"mode": "color"}}   ← uses scenario colour
          {"wallpaper": {"source": "wallpaper_engine", "mode": "color_sync"}}
          {"wallpaper": {"source": "wallpaper_engine", "mode": "wallpaper", "workshop_id": "123456"}}
          {"wallpaper": {"source": "wallpaper_engine", "mode": "playlist", "playlist": "Gaming"}}
        """
        wp = scenario.get("wallpaper")
        if not wp:
            return False

        # Wallpaper Engine integration
        source = wp.get("source", "")
        if source == "wallpaper_engine":
            return self._apply_wallpaper_engine(wp, scenario)

        mode = wp.get("mode", "color")
        color = wp.get("color", scenario.get("color", "#333333"))

        match mode:
            case "image":
                return self.set_image(wp.get("path", ""))
            case "color":
                return self.set_color(color)
            case "gradient":
                return self.set_gradient(color)
            case "url":
                return self.set_from_url(wp.get("url", ""))
            case "restore":
                self.restore_original()
                return True
            case _:
                return self.set_gradient(color)

    def _apply_wallpaper_engine(self, wp: dict, scenario: dict) -> bool:
        """Apply Wallpaper Engine settings."""
        we_path = self._find_wallpaper_engine()
        if not we_path:
            log.debug("Wallpaper Engine not found")
            return False

        mode = wp.get("mode", "color_sync")
        color = wp.get("color", scenario.get("color", "#4A90D9"))

        match mode:
            case "wallpaper":
                wid = wp.get("workshop_id", "")
                path = wp.get("path", "")
                if wid:
                    # Find in Steam workshop
                    for steam_dir in self._we_steam_dirs():
                        wp_path = Path(steam_dir) / str(wid)
                        if wp_path.exists():
                            path = str(wp_path)
                            break
                if path:
                    return self._we_command(we_path, ["-control", "openWallpaper", "-file", path])
                return False

            case "playlist":
                name = wp.get("playlist", "")
                if name:
                    return self._we_command(we_path, ["-control", "playlistPlay", "-playlist", name])
                return False

            case "properties":
                props = wp.get("properties", {})
                if props:
                    import json
                    return self._we_command(we_path, ["-control", "applyProperties", "-properties", json.dumps(props)])
                return False

            case "color_sync" | _:
                # Sync WE scheme colour to scenario colour
                r, g, b = self._hex_to_rgb(color)
                we_color = f"{r/255:.2f} {g/255:.2f} {b/255:.2f}"
                import json
                return self._we_command(we_path, [
                    "-control", "applyProperties",
                    "-properties", json.dumps({"schemecolor": we_color}),
                ])

    def _we_command(self, we_path: str, args: list[str]) -> bool:
        try:
            subprocess.run([we_path] + args, timeout=5, capture_output=True)
            return True
        except Exception:
            return False

    def _find_wallpaper_engine(self) -> str | None:
        if self._system == "Windows":
            for p in [
                r"C:\Program Files (x86)\Steam\steamapps\common\wallpaper_engine\wallpaper64.exe",
                r"D:\Steam\steamapps\common\wallpaper_engine\wallpaper64.exe",
            ]:
                if Path(p).exists():
                    return p
        else:
            if shutil.which("linux-wallpaperengine"):
                return "linux-wallpaperengine"
        return None

    def _we_steam_dirs(self) -> list[str]:
        if self._system == "Windows":
            return [
                r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960",
                r"D:\Steam\steamapps\workshop\content\431960",
            ]
        return [
            str(Path.home() / ".steam/steam/steamapps/workshop/content/431960"),
            str(Path.home() / ".local/share/Steam/steamapps/workshop/content/431960"),
        ]

    # ── Platform-specific implementations ────────────────────────────────────

    def _linux_set(self, path: str) -> bool:
        """Set wallpaper on Linux (GNOME, KDE, Sway, Hyprland, feh)."""
        desktop = os.environ.get("XDG_CURRENT_DESKTOP", "").lower()
        session = os.environ.get("XDG_SESSION_TYPE", "").lower()

        # GNOME / Unity / Cinnamon
        if any(d in desktop for d in ("gnome", "unity", "cinnamon", "budgie")):
            return subprocess.run([
                "gsettings", "set", "org.gnome.desktop.background", "picture-uri",
                f"file://{os.path.abspath(path)}"
            ], timeout=5).returncode == 0

        # KDE Plasma
        if "kde" in desktop or "plasma" in desktop:
            script = f'''
            var allDesktops = desktops();
            for (var i = 0; i < allDesktops.length; i++) {{
                var d = allDesktops[i];
                d.wallpaperPlugin = "org.kde.image";
                d.currentConfigGroup = ["Wallpaper", "org.kde.image", "General"];
                d.writeConfig("Image", "file://{os.path.abspath(path)}");
            }}
            '''
            return subprocess.run([
                "qdbus", "org.kde.plasmashell", "/PlasmaShell",
                "org.kde.PlasmaShell.evaluateScript", script
            ], timeout=5).returncode == 0

        # Sway / Hyprland (wlroots)
        if session == "wayland":
            if shutil.which("swaybg"):
                subprocess.Popen(["swaybg", "-i", path, "-m", "fill"])
                return True
            if shutil.which("hyprpaper"):
                subprocess.run(["hyprctl", "hyprpaper", "wallpaper", f",{path}"], timeout=5)
                return True

        # Fallback: feh (X11)
        if shutil.which("feh"):
            return subprocess.run(["feh", "--bg-fill", path], timeout=5).returncode == 0

        # Fallback: xfconf (XFCE)
        if shutil.which("xfconf-query"):
            return subprocess.run([
                "xfconf-query", "-c", "xfce4-desktop",
                "-p", "/backdrop/screen0/monitor0/workspace0/last-image",
                "-s", path
            ], timeout=5).returncode == 0

        return False

    def _macos_set(self, path: str) -> bool:
        """Set wallpaper on macOS via osascript."""
        script = f'tell application "Finder" to set desktop picture to POSIX file "{os.path.abspath(path)}"'
        return subprocess.run(["osascript", "-e", script], timeout=10).returncode == 0

    def _windows_set(self, path: str) -> bool:
        """Set wallpaper on Windows via ctypes or PowerShell."""
        abs_path = os.path.abspath(path)
        try:
            # Try ctypes (fastest, no subprocess)
            import ctypes
            SPI_SETDESKWALLPAPER = 0x0014
            SPIF_UPDATEINIFILE = 0x01
            SPIF_SENDCHANGE = 0x02
            ctypes.windll.user32.SystemParametersInfoW(
                SPI_SETDESKWALLPAPER, 0, abs_path,
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE
            )
            return True
        except (ImportError, AttributeError):
            # Fallback: PowerShell
            ps = f'''
            Add-Type -TypeDefinition @"
            using System.Runtime.InteropServices;
            public class Wallpaper {{
                [DllImport("user32.dll", CharSet=CharSet.Unicode)]
                public static extern int SystemParametersInfo(int uAction, int uParam, string lpvParam, int fuWinIni);
            }}
"@
            [Wallpaper]::SystemParametersInfo(0x0014, 0, "{abs_path}", 0x01 | 0x02)
            '''
            return subprocess.run(["powershell", "-command", ps], timeout=10).returncode == 0

    def _get_current(self) -> str | None:
        """Get the current wallpaper path."""
        try:
            if self._system == "Linux":
                result = subprocess.run(
                    ["gsettings", "get", "org.gnome.desktop.background", "picture-uri"],
                    capture_output=True, text=True, timeout=5,
                )
                uri = result.stdout.strip().strip("'\"")
                return uri.replace("file://", "") if uri else None
            elif self._system == "Darwin":
                result = subprocess.run(
                    ["osascript", "-e", 'tell application "Finder" to get POSIX path of (desktop picture as alias)'],
                    capture_output=True, text=True, timeout=5,
                )
                return result.stdout.strip() or None
        except Exception:
            pass
        return None

    def _set_color_fallback(self, hex_color: str) -> bool:
        """Set solid colour without Pillow — write a tiny BMP."""
        r, g, b = self._hex_to_rgb(hex_color)
        # 1x1 pixel BMP
        bmp = bytearray(58)
        bmp[0:2] = b"BM"
        bmp[2:6] = (58).to_bytes(4, "little")
        bmp[10:14] = (54).to_bytes(4, "little")
        bmp[14:18] = (40).to_bytes(4, "little")
        bmp[18:22] = (1).to_bytes(4, "little")
        bmp[22:26] = (1).to_bytes(4, "little")
        bmp[26:28] = (1).to_bytes(2, "little")
        bmp[28:30] = (24).to_bytes(2, "little")
        bmp[54] = b; bmp[55] = g; bmp[56] = r; bmp[57] = 0
        path = str(self._temp_dir / "solid.bmp")
        with open(path, "wb") as f:
            f.write(bmp)
        return self.set_image(path)

    @staticmethod
    def _hex_to_rgb(hex_color: str) -> tuple[int, int, int]:
        h = hex_color.lstrip("#")
        return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16))


class OzmaAgent:
    """Main agent loop."""

    def __init__(self, controller_url: str = "") -> None:
        self._controller_url = controller_url
        self._clipboard = ClipboardManager()
        self._display = DisplayInfo()
        self._wallpaper = WallpaperManager()
        self._node_id = ""  # Set by controller
        self._running = True

    async def run(self) -> None:
        # Discover controller
        if not self._controller_url:
            self._controller_url = await self._discover_controller()
        if not self._controller_url:
            log.error("No controller found. Specify with --controller URL")
            return

        log.info("Agent started — controller: %s", self._controller_url)

        # Register with controller
        await self._register()

        # Run clipboard watch + WebSocket listener in parallel
        await asyncio.gather(
            self._clipboard_watch(),
            self._websocket_listen(),
            self._geometry_report_loop(),
        )

    async def _discover_controller(self) -> str:
        """Try to find the controller via mDNS or network scan."""
        # Simple: try common addresses
        for host in ["10.0.100.1", "192.168.1.1", "localhost"]:
            try:
                import urllib.request
                url = f"http://{host}:{CONTROLLER_PORT}/api/v1/status"
                with urllib.request.urlopen(url, timeout=2):
                    return f"http://{host}:{CONTROLLER_PORT}"
            except Exception:
                continue
        return ""

    async def _register(self) -> None:
        """Register this agent with the controller."""
        screens = self._display.get_screens()
        import urllib.request
        data = json.dumps({
            "hostname": platform.node(),
            "platform": platform.system(),
            "screens": screens,
        }).encode()
        try:
            req = urllib.request.Request(
                f"{self._controller_url}/api/v1/agent/register",
                data=data, headers={"Content-Type": "application/json"},
                method="POST",
            )
            urllib.request.urlopen(req, timeout=5)
            log.info("Registered with controller")
        except Exception as e:
            log.warning("Registration failed: %s", e)

    async def _clipboard_watch(self) -> None:
        """Watch clipboard for changes and send to controller."""
        while self._running:
            content = self._clipboard.has_changed()
            if content:
                await self._send_clipboard(content)
            await asyncio.sleep(POLL_INTERVAL)

    async def _send_clipboard(self, content: str) -> None:
        import urllib.request
        try:
            data = json.dumps({"content": content[:65536]}).encode()  # limit to 64KB
            req = urllib.request.Request(
                f"{self._controller_url}/api/v1/agent/clipboard",
                data=data, headers={"Content-Type": "application/json"},
                method="POST",
            )
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=5))
        except Exception:
            pass

    async def _websocket_listen(self) -> None:
        """Listen for controller events (clipboard push, scenario switch, wallpaper)."""
        # Save original wallpaper on startup so we can restore it
        self._wallpaper.save_original()

        while self._running:
            await asyncio.sleep(5)
            try:
                import urllib.request

                # Poll for clipboard push
                try:
                    url = f"{self._controller_url}/api/v1/agent/clipboard"
                    with urllib.request.urlopen(url, timeout=3) as r:
                        data = json.loads(r.read())
                        if data.get("content"):
                            self._clipboard.write(data["content"])
                except Exception:
                    pass

                # Poll for scenario (wallpaper + other settings)
                try:
                    url = f"{self._controller_url}/api/v1/agent/scenario"
                    with urllib.request.urlopen(url, timeout=3) as r:
                        data = json.loads(r.read())
                        scenario = data.get("scenario")
                        if scenario and scenario.get("wallpaper"):
                            self._wallpaper.apply_scenario(scenario)
                except Exception:
                    pass

            except Exception:
                pass

    async def _geometry_report_loop(self) -> None:
        """Periodically report display geometry."""
        while self._running:
            screens = self._display.get_screens()
            try:
                import urllib.request
                data = json.dumps({"screens": screens}).encode()
                req = urllib.request.Request(
                    f"{self._controller_url}/api/v1/agent/geometry",
                    data=data, headers={"Content-Type": "application/json"},
                    method="POST",
                )
                loop = asyncio.get_running_loop()
                await loop.run_in_executor(None, lambda: urllib.request.urlopen(req, timeout=5))
            except Exception:
                pass
            await asyncio.sleep(30)


def main():
    import argparse
    p = argparse.ArgumentParser(description="Ozma Host Agent")
    p.add_argument("--controller", default="", help="Controller URL (auto-discover if empty)")
    p.add_argument("--debug", action="store_true")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.debug else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    agent = OzmaAgent(controller_url=args.controller)
    asyncio.run(agent.run())


if __name__ == "__main__":
    main()
