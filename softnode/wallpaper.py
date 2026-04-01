# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Wallpaper control — set the desktop background from the controller.

Cross-platform wallpaper management for soft nodes. The controller
can push wallpaper changes as part of scenario switching — switch to
"Gaming" and the wallpaper changes on the target machine.

Supported desktops:
  Linux:
    GNOME (gsettings)
    KDE Plasma (qdbus)
    Sway/Hyprland (swaybg)
    XFCE (xfconf-query)
    i3/feh (feh --bg-fill)
    Generic X11 (xwallpaper, nitrogen)
  macOS:
    osascript (AppleScript)
  Windows:
    ctypes (SystemParametersInfoW)

The soft node exposes an API endpoint:
  POST /wallpaper {"url": "...", "path": "...", "color": "#1a1a2e"}

The controller calls this when a scenario activates and the scenario
has a wallpaper configured.
"""

from __future__ import annotations

import logging
import os
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path

log = logging.getLogger("ozma.softnode.wallpaper")


def detect_desktop() -> str:
    """Detect the current desktop environment."""
    system = platform.system()
    if system == "Darwin":
        return "macos"
    if system == "Windows":
        return "windows"

    # Linux desktop detection
    de = os.environ.get("XDG_CURRENT_DESKTOP", "").lower()
    session = os.environ.get("XDG_SESSION_TYPE", "").lower()

    if "gnome" in de or "unity" in de:
        return "gnome"
    if "kde" in de or "plasma" in de:
        return "kde"
    if "xfce" in de:
        return "xfce"
    if "sway" in de or session == "wayland" and shutil.which("swaybg"):
        return "sway"
    if "hyprland" in de:
        return "hyprland"
    if shutil.which("feh"):
        return "feh"
    if shutil.which("xwallpaper"):
        return "xwallpaper"

    return "unknown"


def set_wallpaper(image_path: str) -> bool:
    """Set the desktop wallpaper. Returns True on success."""
    desktop = detect_desktop()
    path = str(Path(image_path).resolve())

    try:
        match desktop:
            case "gnome":
                subprocess.run([
                    "gsettings", "set", "org.gnome.desktop.background",
                    "picture-uri", f"file://{path}"
                ], capture_output=True, check=True)
                subprocess.run([
                    "gsettings", "set", "org.gnome.desktop.background",
                    "picture-uri-dark", f"file://{path}"
                ], capture_output=True)
                return True

            case "kde":
                # KDE Plasma via qdbus
                script = f'''
                var allDesktops = desktops();
                for (var i = 0; i < allDesktops.length; i++) {{
                    var d = allDesktops[i];
                    d.wallpaperPlugin = "org.kde.image";
                    d.currentConfigGroup = ["Wallpaper", "org.kde.image", "General"];
                    d.writeConfig("Image", "file://{path}");
                }}
                '''
                subprocess.run([
                    "qdbus", "org.kde.plasmashell", "/PlasmaShell",
                    "org.kde.PlasmaShell.evaluateScript", script
                ], capture_output=True, check=True)
                return True

            case "xfce":
                subprocess.run([
                    "xfconf-query", "-c", "xfce4-desktop",
                    "-p", "/backdrop/screen0/monitor0/workspace0/last-image",
                    "-s", path
                ], capture_output=True, check=True)
                return True

            case "sway" | "hyprland":
                subprocess.run(["swaybg", "-i", path, "-m", "fill"],
                               capture_output=True)
                return True

            case "feh":
                subprocess.run(["feh", "--bg-fill", path], capture_output=True, check=True)
                return True

            case "xwallpaper":
                subprocess.run(["xwallpaper", "--zoom", path], capture_output=True, check=True)
                return True

            case "macos":
                subprocess.run([
                    "osascript", "-e",
                    f'tell application "Finder" to set desktop picture to POSIX file "{path}"'
                ], capture_output=True, check=True)
                return True

            case "windows":
                import ctypes
                SPI_SETDESKWALLPAPER = 0x0014
                SPIF_UPDATEINIFILE = 0x01
                SPIF_SENDCHANGE = 0x02
                ctypes.windll.user32.SystemParametersInfoW(
                    SPI_SETDESKWALLPAPER, 0, path,
                    SPIF_UPDATEINIFILE | SPIF_SENDCHANGE
                )
                return True

            case _:
                log.warning("Unknown desktop environment: %s", desktop)
                return False

    except Exception as e:
        log.warning("Failed to set wallpaper (%s): %s", desktop, e)
        return False


def set_solid_color(hex_color: str) -> bool:
    """Set a solid colour wallpaper (generates a small image)."""
    try:
        from PIL import Image
        img = Image.new("RGB", (1920, 1080), hex_color)
        tmp = Path(tempfile.gettempdir()) / "ozma-wallpaper-solid.png"
        img.save(str(tmp))
        return set_wallpaper(str(tmp))
    except ImportError:
        log.warning("Pillow not installed — cannot generate solid colour wallpaper")
        return False


async def download_and_set(url: str) -> bool:
    """Download an image from a URL and set it as wallpaper."""
    import urllib.request
    try:
        tmp = Path(tempfile.gettempdir()) / "ozma-wallpaper-download.jpg"
        urllib.request.urlretrieve(url, str(tmp))
        return set_wallpaper(str(tmp))
    except Exception as e:
        log.warning("Failed to download wallpaper from %s: %s", url, e)
        return False


def get_current_wallpaper() -> str:
    """Get the current wallpaper path (best effort)."""
    desktop = detect_desktop()
    try:
        match desktop:
            case "gnome":
                r = subprocess.run(
                    ["gsettings", "get", "org.gnome.desktop.background", "picture-uri"],
                    capture_output=True, text=True,
                )
                return r.stdout.strip().strip("'").replace("file://", "")
            case "kde":
                # KDE is complex — skip for now
                return ""
            case "macos":
                r = subprocess.run(
                    ["osascript", "-e", 'tell application "Finder" to get POSIX path of (desktop picture as alias)'],
                    capture_output=True, text=True,
                )
                return r.stdout.strip()
            case _:
                return ""
    except Exception:
        return ""
