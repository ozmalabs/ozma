# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Wallpaper sources — fetch backgrounds from local, cloud, and API providers.

Provides desktop backgrounds for scenario wallpaper changes and for the
screen renderer (status panel backgrounds, ambient displays, etc.).

Built-in sources:
  local         — files from a directory on the controller
  url           — single URL (direct image link)
  immich        — self-hosted Immich photo server (API)
  google_photos — Google Photos API (OAuth)
  icloud        — iCloud Photos (via pyicloud or web scrape)
  unsplash      — Unsplash API (free, attribution required)
  pexels        — Pexels API (free, attribution required)
  bing_daily    — Bing daily wallpaper (free, no API key)
  reddit        — r/wallpapers, r/EarthPorn, etc. (Reddit JSON API)
  displayfusion — DisplayFusion online wallpaper sources
  solid         — generated solid colour
  gradient      — generated gradient from scenario colour
  pattern       — generated geometric pattern from scenario colour

Each source can be:
  - Public (free, no auth) — Unsplash, Pexels, Bing daily, Reddit
  - Private (self-hosted) — Immich, local directory
  - Paid/auth (API key or OAuth) — Google Photos, iCloud

Sources are configured per-scenario:
  {"wallpaper": {"source": "immich", "album": "Desk Backgrounds", "mode": "random"}}
  {"wallpaper": {"source": "unsplash", "query": "dark minimalist", "orientation": "landscape"}}
  {"wallpaper": {"source": "local", "path": "~/wallpapers/gaming/"}}
  {"wallpaper": {"source": "gradient"}}   ← uses scenario colour

The source system is pluggable — add new sources by implementing
WallpaperSource and registering in SOURCES.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import logging
import os
import random
import time
import urllib.request
import urllib.error
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("ozma.wallpaper_sources")

CACHE_DIR = Path("/tmp/ozma-wallpaper-cache")
CACHE_DIR.mkdir(parents=True, exist_ok=True)


@dataclass
class WallpaperImage:
    """A resolved wallpaper image."""
    path: str                    # Local file path (after download/cache)
    source: str                  # Source ID
    url: str = ""                # Original URL (if from web)
    attribution: str = ""        # Credit line (for free APIs)
    width: int = 0
    height: int = 0


class WallpaperSource:
    """Base class for wallpaper sources."""

    source_id: str = ""
    name: str = ""
    requires_auth: bool = False
    is_free: bool = True

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        """Fetch a wallpaper image based on config. Returns local path."""
        return None

    def to_dict(self) -> dict[str, Any]:
        return {"id": self.source_id, "name": self.name,
                "requires_auth": self.requires_auth, "is_free": self.is_free}


# ── Built-in sources ─────────────────────────────────────────────────────────

class LocalSource(WallpaperSource):
    """Wallpapers from a local directory."""
    source_id = "local"
    name = "Local Files"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        path = config.get("path", "")
        p = Path(path)
        if p.is_file():
            return WallpaperImage(path=str(p), source=self.source_id)
        if p.is_dir():
            images = [f for f in p.iterdir()
                      if f.suffix.lower() in (".jpg", ".jpeg", ".png", ".webp", ".bmp")]
            if not images:
                return None
            mode = config.get("mode", "random")
            if mode == "random":
                chosen = random.choice(images)
            else:  # sequential
                chosen = images[int(time.time()) % len(images)]
            return WallpaperImage(path=str(chosen), source=self.source_id)
        return None


class URLSource(WallpaperSource):
    """Single URL wallpaper."""
    source_id = "url"
    name = "Direct URL"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        url = config.get("url", "")
        if not url:
            return None
        path = await _download_cached(url)
        return WallpaperImage(path=path, source=self.source_id, url=url) if path else None


class UnsplashSource(WallpaperSource):
    """Unsplash free wallpapers (requires API key for high volume)."""
    source_id = "unsplash"
    name = "Unsplash"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        query = config.get("query", "dark minimalist desktop")
        orientation = config.get("orientation", "landscape")
        api_key = config.get("api_key", "")

        if api_key:
            url = f"https://api.unsplash.com/photos/random?query={query}&orientation={orientation}&client_id={api_key}"
        else:
            url = f"https://source.unsplash.com/1920x1080/?{query}"
            path = await _download_cached(url, max_age=3600)
            return WallpaperImage(path=path, source=self.source_id, url=url,
                                  attribution="Photo from Unsplash") if path else None

        try:
            data = await _fetch_json(url)
            img_url = data.get("urls", {}).get("full", data.get("urls", {}).get("regular", ""))
            author = data.get("user", {}).get("name", "Unknown")
            if img_url:
                path = await _download_cached(img_url)
                return WallpaperImage(
                    path=path, source=self.source_id, url=img_url,
                    attribution=f"Photo by {author} on Unsplash",
                    width=data.get("width", 0), height=data.get("height", 0),
                ) if path else None
        except Exception:
            pass
        return None


class PexelsSource(WallpaperSource):
    """Pexels free wallpapers (API key required)."""
    source_id = "pexels"
    name = "Pexels"
    requires_auth = True

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        api_key = config.get("api_key", "")
        if not api_key:
            return None
        query = config.get("query", "dark desktop background")
        try:
            data = await _fetch_json(
                f"https://api.pexels.com/v1/search?query={query}&per_page=15&orientation=landscape",
                headers={"Authorization": api_key},
            )
            photos = data.get("photos", [])
            if not photos:
                return None
            photo = random.choice(photos)
            img_url = photo.get("src", {}).get("original", photo.get("src", {}).get("large2x", ""))
            photographer = photo.get("photographer", "Unknown")
            if img_url:
                path = await _download_cached(img_url)
                return WallpaperImage(
                    path=path, source=self.source_id, url=img_url,
                    attribution=f"Photo by {photographer} on Pexels",
                ) if path else None
        except Exception:
            pass
        return None


class BingDailySource(WallpaperSource):
    """Bing daily wallpaper — free, no API key needed."""
    source_id = "bing_daily"
    name = "Bing Daily Wallpaper"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        try:
            data = await _fetch_json("https://www.bing.com/HPImageArchive.aspx?format=js&idx=0&n=1&mkt=en-US")
            images = data.get("images", [])
            if not images:
                return None
            url = "https://www.bing.com" + images[0].get("url", "")
            copyright_text = images[0].get("copyright", "")
            path = await _download_cached(url, max_age=86400)
            return WallpaperImage(
                path=path, source=self.source_id, url=url,
                attribution=copyright_text,
            ) if path else None
        except Exception:
            return None


class RedditSource(WallpaperSource):
    """Reddit wallpaper subreddits (public JSON API)."""
    source_id = "reddit"
    name = "Reddit"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        subreddit = config.get("subreddit", "wallpapers")
        sort = config.get("sort", "hot")
        try:
            url = f"https://www.reddit.com/r/{subreddit}/{sort}.json?limit=25"
            data = await _fetch_json(url, headers={"User-Agent": "ozma-wallpaper/1.0"})
            posts = [p["data"] for p in data.get("data", {}).get("children", [])
                     if p["data"].get("post_hint") == "image"]
            if not posts:
                return None
            post = random.choice(posts)
            img_url = post.get("url", "")
            if img_url and any(img_url.endswith(ext) for ext in (".jpg", ".jpeg", ".png", ".webp")):
                path = await _download_cached(img_url)
                return WallpaperImage(
                    path=path, source=self.source_id, url=img_url,
                    attribution=f"r/{subreddit}: {post.get('title', '')[:60]}",
                ) if path else None
        except Exception:
            pass
        return None


class ImmichSource(WallpaperSource):
    """Immich self-hosted photo server."""
    source_id = "immich"
    name = "Immich (Self-Hosted)"
    requires_auth = True

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        base_url = config.get("url", "").rstrip("/")
        api_key = config.get("api_key", "")
        album = config.get("album", "")
        if not base_url or not api_key:
            return None
        try:
            headers = {"x-api-key": api_key}
            if album:
                data = await _fetch_json(f"{base_url}/api/albums?searchTerm={album}", headers=headers)
                if data and isinstance(data, list) and data:
                    album_id = data[0].get("id", "")
                    album_data = await _fetch_json(f"{base_url}/api/albums/{album_id}", headers=headers)
                    assets = album_data.get("assets", [])
                else:
                    return None
            else:
                data = await _fetch_json(f"{base_url}/api/assets/random?count=1", headers=headers)
                assets = data if isinstance(data, list) else [data]

            if not assets:
                return None
            asset = random.choice(assets)
            asset_id = asset.get("id", "")
            img_url = f"{base_url}/api/assets/{asset_id}/original"
            path = await _download_cached(img_url, headers=headers)
            return WallpaperImage(path=path, source=self.source_id, url=img_url) if path else None
        except Exception:
            return None


class SolidColorSource(WallpaperSource):
    """Generated solid colour from scenario colour."""
    source_id = "solid"
    name = "Solid Colour"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        color = config.get("color", scenario_color or "#333333")
        try:
            from PIL import Image
            img = Image.new("RGB", (1920, 1080), _hex_rgb(color))
            path = str(CACHE_DIR / f"solid-{color.lstrip('#')}.png")
            img.save(path)
            return WallpaperImage(path=path, source=self.source_id)
        except ImportError:
            return None


class GradientSource(WallpaperSource):
    """Generated gradient from scenario colour."""
    source_id = "gradient"
    name = "Gradient"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        color = config.get("color", scenario_color or "#333333")
        try:
            from PIL import Image, ImageDraw
            import math
            r, g, b = _hex_rgb(color)
            img = Image.new("RGB", (1920, 1080))
            draw = ImageDraw.Draw(img)
            for y in range(1080):
                t = math.sin(y / 1080.0 * math.pi) * 0.5 + 0.08
                draw.line([(0, y), (1920, y)], fill=(int(r * t), int(g * t), int(b * t)))
            path = str(CACHE_DIR / f"gradient-{color.lstrip('#')}.png")
            img.save(path)
            return WallpaperImage(path=path, source=self.source_id)
        except ImportError:
            return None


class PatternSource(WallpaperSource):
    """Generated geometric pattern from scenario colour."""
    source_id = "pattern"
    name = "Geometric Pattern"

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        color = config.get("color", scenario_color or "#333333")
        style = config.get("style", "hexgrid")  # hexgrid, dots, lines, triangles
        try:
            from PIL import Image, ImageDraw
            import math
            r, g, b = _hex_rgb(color)
            img = Image.new("RGB", (1920, 1080), (int(r * 0.08), int(g * 0.08), int(b * 0.08)))
            draw = ImageDraw.Draw(img)
            dim = (int(r * 0.15), int(g * 0.15), int(b * 0.15))

            if style == "hexgrid":
                size = 40
                for row in range(0, 1080 + size, int(size * 1.5)):
                    offset = size if (row // int(size * 1.5)) % 2 else 0
                    for col in range(offset, 1920 + size, size * 2):
                        for i in range(6):
                            a1 = math.radians(60 * i)
                            a2 = math.radians(60 * (i + 1))
                            draw.line([
                                (col + size * 0.4 * math.cos(a1), row + size * 0.4 * math.sin(a1)),
                                (col + size * 0.4 * math.cos(a2), row + size * 0.4 * math.sin(a2)),
                            ], fill=dim, width=1)
            elif style == "dots":
                for y in range(0, 1080, 30):
                    for x in range(0, 1920, 30):
                        draw.ellipse([x - 2, y - 2, x + 2, y + 2], fill=dim)
            elif style == "lines":
                for x in range(-1080, 1920, 40):
                    draw.line([(x, 0), (x + 1080, 1080)], fill=dim, width=1)

            path = str(CACHE_DIR / f"pattern-{style}-{color.lstrip('#')}.png")
            img.save(path)
            return WallpaperImage(path=path, source=self.source_id)
        except ImportError:
            return None


# ── Source registry ──────────────────────────────────────────────────────────

class WallpaperEngineSource(WallpaperSource):
    """
    Wallpaper Engine integration — switch animated wallpapers per scenario.

    Wallpaper Engine (Steam) stores wallpapers in:
      Steam/steamapps/workshop/content/431960/{workshop_id}/

    Integration modes:
      1. Direct: tell WE to switch wallpaper via its CLI/IPC
      2. Preset: save WE playlists per scenario, activate on switch
      3. Property: set WE wallpaper properties (colours, speed) to match scenario

    On Windows: Wallpaper Engine has a command-line interface:
      wallpaper64.exe -control openWallpaper -file "<path>"
      wallpaper64.exe -control applyProperties -properties '{"schemecolor":"0.29 0.56 0.85"}'
      wallpaper64.exe -control playlistPlay -playlist "<name>"

    On Linux: Wallpaper Engine has a linux-wallpaperengine project
      (github.com/Almamu/linux-wallpaperengine) and a Plasma plugin.

    This source runs on the host agent, not the controller — the agent
    receives the scenario switch event and commands WE locally.
    """
    source_id = "wallpaper_engine"
    name = "Wallpaper Engine"
    requires_auth = False

    # Default Steam paths per platform
    _STEAM_PATHS = {
        "Windows": [
            r"C:\Program Files (x86)\Steam\steamapps\workshop\content\431960",
            r"D:\Steam\steamapps\workshop\content\431960",
        ],
        "Linux": [
            os.path.expanduser("~/.steam/steam/steamapps/workshop/content/431960"),
            os.path.expanduser("~/.local/share/Steam/steamapps/workshop/content/431960"),
        ],
    }

    async def get_image(self, config: dict, scenario_color: str = "") -> WallpaperImage | None:
        """
        For Wallpaper Engine, we don't return a static image — instead we
        return a command payload that the host agent executes.

        Config options:
          workshop_id:  Steam Workshop ID to activate
          playlist:     WE playlist name to activate
          properties:   WE properties to set (scheme colour, speed, etc.)
          path:         Direct path to a .pkg or scene.json
          mode:         "wallpaper" (set specific), "playlist" (activate playlist),
                        "properties" (set properties on current), "color_sync" (match scenario colour)
        """
        mode = config.get("mode", "color_sync")
        result_path = ""

        if mode == "wallpaper" and config.get("workshop_id"):
            # Return the workshop wallpaper path
            wid = config["workshop_id"]
            for steam_dir in self._STEAM_PATHS.get(os.name == "nt" and "Windows" or "Linux", []):
                wp_dir = Path(steam_dir) / str(wid)
                if wp_dir.exists():
                    result_path = str(wp_dir)
                    break

        # Return a special WallpaperImage that the agent knows to handle via WE
        return WallpaperImage(
            path=result_path,
            source=self.source_id,
            url="",
            attribution="",
        )


class WallpaperEngineAgent:
    """
    Wallpaper Engine control — runs on the host agent side.

    Receives scenario switch events from the controller and commands
    Wallpaper Engine to change wallpapers, playlists, or properties.
    """

    def __init__(self) -> None:
        self._we_path = self._find_wallpaper_engine()
        self._available = self._we_path is not None

    @property
    def available(self) -> bool:
        return self._available

    def apply_scenario(self, scenario: dict) -> bool:
        """Apply Wallpaper Engine settings for a scenario."""
        wp = scenario.get("wallpaper", {})
        if wp.get("source") != "wallpaper_engine":
            return False

        mode = wp.get("mode", "color_sync")
        color = wp.get("color", scenario.get("color", "#4A90D9"))

        match mode:
            case "wallpaper":
                return self._open_wallpaper(wp.get("workshop_id", ""), wp.get("path", ""))
            case "playlist":
                return self._activate_playlist(wp.get("playlist", ""))
            case "properties":
                return self._set_properties(wp.get("properties", {}))
            case "color_sync":
                return self._sync_scheme_color(color)
            case _:
                return self._sync_scheme_color(color)

    def _open_wallpaper(self, workshop_id: str = "", path: str = "") -> bool:
        """Open a specific wallpaper by workshop ID or path."""
        if not self._we_path:
            return False

        if workshop_id:
            # Find the wallpaper in Steam workshop
            for steam_dir in WallpaperEngineSource._STEAM_PATHS.get(
                "Windows" if os.name == "nt" else "Linux", []
            ):
                wp_path = Path(steam_dir) / str(workshop_id)
                if wp_path.exists():
                    path = str(wp_path)
                    break

        if not path:
            return False

        try:
            subprocess.run(
                [self._we_path, "-control", "openWallpaper", "-file", path],
                timeout=5, capture_output=True,
            )
            return True
        except Exception:
            return False

    def _activate_playlist(self, playlist_name: str) -> bool:
        """Activate a named WE playlist."""
        if not self._we_path or not playlist_name:
            return False
        try:
            subprocess.run(
                [self._we_path, "-control", "playlistPlay", "-playlist", playlist_name],
                timeout=5, capture_output=True,
            )
            return True
        except Exception:
            return False

    def _set_properties(self, properties: dict) -> bool:
        """Set wallpaper properties (colours, speed, etc.)."""
        if not self._we_path or not properties:
            return False
        try:
            import json
            props_json = json.dumps(properties)
            subprocess.run(
                [self._we_path, "-control", "applyProperties", "-properties", props_json],
                timeout=5, capture_output=True,
            )
            return True
        except Exception:
            return False

    def _sync_scheme_color(self, hex_color: str) -> bool:
        """
        Set Wallpaper Engine's scheme colour to match the scenario colour.

        WE scheme colour format: "R G B" as floats 0.0-1.0
        Many WE wallpapers respond to scheme colour changes.
        """
        r, g, b = _hex_rgb(hex_color)
        we_color = f"{r/255:.2f} {g/255:.2f} {b/255:.2f}"
        return self._set_properties({"schemecolor": we_color})

    def _find_wallpaper_engine(self) -> str | None:
        """Find the Wallpaper Engine executable."""
        if os.name == "nt":
            candidates = [
                r"C:\Program Files (x86)\Steam\steamapps\common\wallpaper_engine\wallpaper64.exe",
                r"C:\Program Files (x86)\Steam\steamapps\common\wallpaper_engine\wallpaper32.exe",
                r"D:\Steam\steamapps\common\wallpaper_engine\wallpaper64.exe",
            ]
            for c in candidates:
                if Path(c).exists():
                    return c
        else:
            # Linux: look for linux-wallpaperengine
            if shutil.which("linux-wallpaperengine"):
                return "linux-wallpaperengine"
        return None


SOURCES: dict[str, WallpaperSource] = {
    "local": LocalSource(),
    "url": URLSource(),
    "unsplash": UnsplashSource(),
    "pexels": PexelsSource(),
    "bing_daily": BingDailySource(),
    "reddit": RedditSource(),
    "immich": ImmichSource(),
    "solid": SolidColorSource(),
    "gradient": GradientSource(),
    "pattern": PatternSource(),
    "wallpaper_engine": WallpaperEngineSource(),
}


def list_sources() -> list[dict[str, Any]]:
    return [s.to_dict() for s in SOURCES.values()]


async def resolve_wallpaper(config: dict, scenario_color: str = "") -> WallpaperImage | None:
    """Resolve a wallpaper config to a local image file."""
    source_id = config.get("source", config.get("mode", "gradient"))
    source = SOURCES.get(source_id)
    if not source:
        source = SOURCES.get("gradient")
    return await source.get_image(config, scenario_color)


# ── Helpers ──────────────────────────────────────────────────────────────────

async def _download_cached(url: str, max_age: int = 86400, headers: dict | None = None) -> str | None:
    """Download a URL to cache, return local path."""
    url_hash = hashlib.md5(url.encode()).hexdigest()[:12]
    ext = ".jpg"
    for e in (".png", ".webp", ".jpeg"):
        if e in url.lower():
            ext = e
            break
    cached = CACHE_DIR / f"{url_hash}{ext}"

    if cached.exists() and (time.time() - cached.stat().st_mtime) < max_age:
        return str(cached)

    try:
        loop = asyncio.get_running_loop()
        def _dl():
            req = urllib.request.Request(url)
            if headers:
                for k, v in headers.items():
                    req.add_header(k, v)
            with urllib.request.urlopen(req, timeout=15) as r:
                data = r.read()
                cached.write_bytes(data)
            return str(cached)
        return await loop.run_in_executor(None, _dl)
    except Exception:
        return None


async def _fetch_json(url: str, headers: dict | None = None) -> Any:
    """Fetch JSON from a URL."""
    loop = asyncio.get_running_loop()
    def _f():
        req = urllib.request.Request(url)
        if headers:
            for k, v in headers.items():
                req.add_header(k, v)
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read())
    return await loop.run_in_executor(None, _f)


def _hex_rgb(hex_color: str) -> tuple[int, int, int]:
    h = hex_color.lstrip("#")
    return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16))
