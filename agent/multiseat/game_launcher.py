# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Seat session launcher for multi-seat.

Launches sessions on seats based on their profile: full desktop, game
library (Playnite/Lutris/Steam), single app, kiosk browser, media
player, or a custom command. Also discovers installed games for the
game library profiles.

A seat is just an isolated display with input — what runs on it is
determined by its profile's launcher type.

Usage:
    launcher = GameLauncher(seat_manager)
    await launcher.launch_seat_session(seat)  # uses seat's profile
    games = await launcher.discover_games()   # for game library profiles
    await launcher.launch(games[0], seat)     # launch specific game
    await launcher.stop(seat)
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import platform
import re
import shutil
import signal
import sqlite3
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from .encoder_allocator import EncoderHints
    from .seat import Seat
    from .seat_manager import SeatManager

log = logging.getLogger("ozma.agent.multiseat.launcher")

_which = shutil.which


# ── Data model ────────────────────────────────────────────────────────────────

@dataclass
class GameInfo:
    """A discovered game from any launcher."""
    id: str                     # unique ID (GUID, slug, or appid)
    name: str
    platform: str               # "steam", "gog", "epic", "lutris", "playnite", etc.
    install_path: str
    icon_path: str | None = None
    playtime_minutes: int = 0
    source: str = ""            # "playnite", "lutris", "steam"

    def to_dict(self) -> dict:
        return {
            "id": self.id,
            "name": self.name,
            "platform": self.platform,
            "install_path": self.install_path,
            "icon_path": self.icon_path,
            "playtime_minutes": self.playtime_minutes,
            "source": self.source,
        }


@dataclass
class _SeatGameState:
    """Tracks a running game on a seat."""
    game: GameInfo
    process: asyncio.subprocess.Process | None = None
    pid: int = 0
    launched_at: float = 0.0


# ── VDF parser (Valve Data Format) ───────────────────────────────────────────

def _parse_vdf(text: str) -> dict:
    """
    Simple recursive parser for Valve Data Format files.

    VDF is a nested key-value format:
      "key" "value"
      "key" { ... }
    """
    result: dict[str, Any] = {}
    stack: list[dict] = [result]
    tokens = _vdf_tokenize(text)
    i = 0
    while i < len(tokens):
        token = tokens[i]
        if token == "{":
            i += 1
            continue
        if token == "}":
            if len(stack) > 1:
                stack.pop()
            i += 1
            continue

        # Key token — next is either a value string or "{"
        key = token
        i += 1
        if i >= len(tokens):
            break

        if tokens[i] == "{":
            # Nested section
            child: dict[str, Any] = {}
            stack[-1][key] = child
            stack.append(child)
            i += 1
        else:
            # Key-value pair
            stack[-1][key] = tokens[i]
            i += 1

    return result


def _vdf_tokenize(text: str) -> list[str]:
    """Tokenize VDF text into strings and braces."""
    tokens: list[str] = []
    i = 0
    length = len(text)
    while i < length:
        c = text[i]
        if c in " \t\r\n":
            i += 1
        elif c == "/" and i + 1 < length and text[i + 1] == "/":
            # Line comment
            while i < length and text[i] != "\n":
                i += 1
        elif c in "{}":
            tokens.append(c)
            i += 1
        elif c == '"':
            # Quoted string
            i += 1
            start = i
            while i < length and text[i] != '"':
                if text[i] == "\\" and i + 1 < length:
                    i += 2
                else:
                    i += 1
            tokens.append(text[start:i])
            if i < length:
                i += 1  # skip closing quote
        else:
            # Unquoted token (some VDF files use these)
            start = i
            while i < length and text[i] not in ' \t\r\n{}\"':
                i += 1
            tokens.append(text[start:i])

    return tokens


# ── Steam discovery ──────────────────────────────────────────────────────────

def _find_steam_dir() -> Path | None:
    """Find Steam installation directory."""
    system = platform.system()

    if system == "Linux":
        candidates = [
            Path.home() / ".steam" / "steam",
            Path.home() / ".local" / "share" / "Steam",
            Path("/usr/share/steam"),
        ]
    elif system == "Windows":
        candidates = [
            Path(os.environ.get("ProgramFiles(x86)", "C:\\Program Files (x86)")) / "Steam",
            Path(os.environ.get("ProgramFiles", "C:\\Program Files")) / "Steam",
        ]
    else:
        return None

    for path in candidates:
        if path.is_dir():
            return path
    return None


def _discover_steam_games(steam_dir: Path) -> list[GameInfo]:
    """Discover installed Steam games from libraryfolders.vdf and appmanifest files."""
    games: list[GameInfo] = []

    # Find all library folders
    vdf_path = steam_dir / "steamapps" / "libraryfolders.vdf"
    if not vdf_path.exists():
        # Try older location
        vdf_path = steam_dir / "SteamApps" / "libraryfolders.vdf"

    library_paths: list[Path] = [steam_dir]

    if vdf_path.exists():
        try:
            text = vdf_path.read_text(errors="replace")
            data = _parse_vdf(text)
            # libraryfolders.vdf has numbered keys ("0", "1", ...) each with "path"
            folders = data.get("libraryfolders") or data.get("LibraryFolders") or {}
            for key, entry in folders.items():
                if isinstance(entry, dict) and "path" in entry:
                    lib_path = Path(entry["path"])
                    if lib_path.is_dir() and lib_path != steam_dir:
                        library_paths.append(lib_path)
        except Exception as e:
            log.debug("Failed to parse libraryfolders.vdf: %s", e)

    # Scan each library folder for appmanifest files
    for lib_path in library_paths:
        steamapps = lib_path / "steamapps"
        if not steamapps.is_dir():
            steamapps = lib_path / "SteamApps"
        if not steamapps.is_dir():
            continue

        for manifest in steamapps.glob("appmanifest_*.acf"):
            try:
                text = manifest.read_text(errors="replace")
                data = _parse_vdf(text)
                app_state = data.get("AppState") or data.get("appstate") or {}
                appid = app_state.get("appid", "")
                name = app_state.get("name", "")
                install_dir = app_state.get("installdir", "")

                if not appid or not name:
                    continue

                full_path = str(steamapps / "common" / install_dir) if install_dir else ""

                games.append(GameInfo(
                    id=str(appid),
                    name=name,
                    platform="steam",
                    install_path=full_path,
                    source="steam",
                ))
            except Exception as e:
                log.debug("Failed to parse %s: %s", manifest, e)

    return games


# ── Playnite discovery (Windows) ─────────────────────────────────────────────

def _discover_playnite_games() -> list[GameInfo]:
    """Discover games from Playnite library (Windows only)."""
    if platform.system() != "Windows":
        return []

    games: list[GameInfo] = []
    appdata = os.environ.get("APPDATA", "")
    if not appdata:
        return []

    playnite_dir = Path(appdata) / "Playnite" / "library"
    if not playnite_dir.is_dir():
        return []

    # Try SQLite database first
    db_path = playnite_dir / "games.db"
    if db_path.exists():
        games = _discover_playnite_sqlite(db_path)
        if games:
            return games

    # Fall back to individual JSON files in the games/ subdirectory
    games_dir = playnite_dir / "games"
    if not games_dir.is_dir():
        # Try reading JSON files directly in the library dir
        games_dir = playnite_dir

    for json_file in games_dir.glob("*.json"):
        try:
            data = json.loads(json_file.read_text(errors="replace"))
            if not isinstance(data, dict):
                continue

            game_id = data.get("Id") or data.get("GameId") or json_file.stem
            name = data.get("Name", "")
            if not name:
                continue

            is_installed = data.get("IsInstalled", False)
            if not is_installed:
                continue

            install_dir = data.get("InstallDirectory", "")
            icon = data.get("Icon", "")
            playtime = data.get("Playtime", 0)  # seconds
            platform_name = ""
            if isinstance(data.get("Platform"), dict):
                platform_name = data["Platform"].get("Name", "").lower()
            elif isinstance(data.get("Platforms"), list) and data["Platforms"]:
                p = data["Platforms"][0]
                platform_name = p.get("Name", "").lower() if isinstance(p, dict) else ""

            # Try to determine the sub-platform
            plugin_id = data.get("PluginId", "")
            source = _playnite_plugin_to_source(plugin_id) or platform_name or "playnite"

            games.append(GameInfo(
                id=str(game_id),
                name=name,
                platform=source,
                install_path=install_dir,
                icon_path=icon or None,
                playtime_minutes=playtime // 60 if playtime else 0,
                source="playnite",
            ))
        except Exception as e:
            log.debug("Failed to parse Playnite game %s: %s", json_file, e)

    return games


def _discover_playnite_sqlite(db_path: Path) -> list[GameInfo]:
    """Read Playnite SQLite database for games."""
    games: list[GameInfo] = []
    try:
        conn = sqlite3.connect(str(db_path), timeout=5)
        conn.row_factory = sqlite3.Row
        cursor = conn.execute(
            "SELECT * FROM Games WHERE IsInstalled = 1"
        )
        for row in cursor:
            game_id = row["Id"] if "Id" in row.keys() else ""
            name = row["Name"] if "Name" in row.keys() else ""
            if not name:
                continue

            install_dir = row["InstallDirectory"] if "InstallDirectory" in row.keys() else ""
            icon = row["Icon"] if "Icon" in row.keys() else ""
            playtime = row["Playtime"] if "Playtime" in row.keys() else 0

            games.append(GameInfo(
                id=str(game_id),
                name=name,
                platform="playnite",
                install_path=install_dir or "",
                icon_path=icon or None,
                playtime_minutes=(playtime or 0) // 60,
                source="playnite",
            ))
        conn.close()
    except Exception as e:
        log.debug("Failed to read Playnite database: %s", e)
    return games


def _playnite_plugin_to_source(plugin_id: str) -> str:
    """Map common Playnite plugin GUIDs to source names."""
    # Well-known Playnite plugin IDs
    known = {
        "cb91dfc9-b977-43bf-8e70-55f46e410fab": "steam",
        "aebe8b7c-6dc3-4a66-af31-e7375c6b5e9e": "gog",
        "00000002-dbd1-46c6-b5d0-b1ba559d10e4": "epic",
        "e3c26a3d-d695-4cb7-a769-5ff7612c7edd": "battlenet",
        "c2f038e5-8b92-4877-91f1-da9094155fc5": "ubisoft",
        "88409022-088a-4de8-805a-fdbac291f00a": "origin",
        "7e4fbb5e-2ae3-48d4-8ba0-6b30e7a4e287": "xbox",
    }
    return known.get(plugin_id, "")


# ── Lutris discovery (Linux) ─────────────────────────────────────────────────

def _discover_lutris_games() -> list[GameInfo]:
    """Discover games from Lutris database (Linux only)."""
    if platform.system() != "Linux":
        return []

    games: list[GameInfo] = []
    db_path = Path.home() / ".local" / "share" / "lutris" / "pga.db"
    if not db_path.exists():
        return []

    try:
        conn = sqlite3.connect(str(db_path), timeout=5)
        conn.row_factory = sqlite3.Row
        cursor = conn.execute(
            "SELECT * FROM games WHERE installed = 1"
        )
        for row in cursor:
            slug = row["slug"] if "slug" in row.keys() else ""
            name = row["name"] if "name" in row.keys() else ""
            if not name or not slug:
                continue

            runner = row["runner"] if "runner" in row.keys() else ""
            directory = row["directory"] if "directory" in row.keys() else ""
            playtime = row["playtime"] if "playtime" in row.keys() else 0.0

            # Determine platform from runner
            platform_name = runner or "lutris"

            games.append(GameInfo(
                id=slug,
                name=name,
                platform=platform_name,
                install_path=directory or "",
                playtime_minutes=int((playtime or 0.0) * 60),
                source="lutris",
            ))
        conn.close()
    except Exception as e:
        log.debug("Failed to read Lutris database: %s", e)

    return games


# ── Game Launcher ────────────────────────────────────────────────────────────

class GameLauncher:
    """
    Discovers installed games and launches them on specific seats.

    Sets the correct environment for each seat (display, audio, input)
    and notifies the encoder allocator when a game starts/stops so it
    can rebalance GPU encoder assignments.
    """

    def __init__(self, seat_manager: SeatManager) -> None:
        self._seat_manager = seat_manager
        self._games: list[GameInfo] = []
        self._running: dict[str, _SeatGameState] = {}  # seat_name -> state
        self._discovery_lock = asyncio.Lock()

    @property
    def games(self) -> list[GameInfo]:
        return list(self._games)

    def running_game(self, seat_name: str) -> GameInfo | None:
        """Return the game currently running on a seat, or None."""
        state = self._running.get(seat_name)
        return state.game if state else None

    def running_state(self, seat_name: str) -> dict | None:
        """Return running game state as a dict for API responses."""
        state = self._running.get(seat_name)
        if not state:
            return None
        alive = state.process is not None and state.process.returncode is None
        return {
            "game": state.game.to_dict(),
            "pid": state.pid,
            "alive": alive,
            "launched_at": state.launched_at,
        }

    async def discover_games(self) -> list[GameInfo]:
        """
        Scan all available game sources for installed games.

        Returns the combined deduplicated list. Thread-safe via lock.
        """
        async with self._discovery_lock:
            loop = asyncio.get_running_loop()
            games = await loop.run_in_executor(None, self._discover_sync)
            self._games = games
            log.info("Discovered %d installed games", len(games))
            return list(games)

    def _discover_sync(self) -> list[GameInfo]:
        """Synchronous game discovery (runs in executor)."""
        all_games: list[GameInfo] = []
        seen_ids: set[str] = set()

        # Steam (both platforms)
        try:
            steam_dir = _find_steam_dir()
            if steam_dir:
                steam_games = _discover_steam_games(steam_dir)
                for g in steam_games:
                    key = f"steam:{g.id}"
                    if key not in seen_ids:
                        seen_ids.add(key)
                        all_games.append(g)
                log.info("Steam: %d games", len(steam_games))
        except Exception as e:
            log.debug("Steam discovery failed: %s", e)

        # Playnite (Windows)
        try:
            playnite_games = _discover_playnite_games()
            for g in playnite_games:
                key = f"playnite:{g.id}"
                if key not in seen_ids:
                    seen_ids.add(key)
                    all_games.append(g)
            if playnite_games:
                log.info("Playnite: %d games", len(playnite_games))
        except Exception as e:
            log.debug("Playnite discovery failed: %s", e)

        # Lutris (Linux)
        try:
            lutris_games = _discover_lutris_games()
            for g in lutris_games:
                key = f"lutris:{g.id}"
                if key not in seen_ids:
                    seen_ids.add(key)
                    all_games.append(g)
            if lutris_games:
                log.info("Lutris: %d games", len(lutris_games))
        except Exception as e:
            log.debug("Lutris discovery failed: %s", e)

        all_games.sort(key=lambda g: g.name.lower())
        return all_games

    # ── Seat session launcher ─────────────────────────────────────────────
    # Launches the appropriate session based on the seat's profile.
    # A seat profile's `launcher` field determines what runs:
    #   desktop  → full desktop environment (no-op on existing DE, or start one)
    #   playnite → Playnite in fullscreen (auto-detects: playnite → lutris → steam)
    #   lutris   → Lutris
    #   steam    → Steam Big Picture
    #   app      → single application from launcher_command
    #   custom   → arbitrary command from launcher_command

    async def launch_seat_session(self, seat: "Seat") -> asyncio.subprocess.Process | None:
        """Start the seat's session based on its profile launcher type."""
        from .seat_profiles import SeatProfile
        profile: SeatProfile = seat.profile if hasattr(seat, "profile") else None
        if not profile:
            log.debug("Seat %s has no profile, skipping session launch", seat.name)
            return None

        launcher = profile.launcher
        log.info("Seat %s: starting %s session (profile=%s)", seat.name, launcher, profile.name)

        match launcher:
            case "desktop":
                return await self._launch_desktop(seat, profile)
            case "playnite":
                return await self._launch_game_library(seat, profile, "playnite")
            case "lutris":
                return await self._launch_game_library(seat, profile, "lutris")
            case "steam":
                return await self._launch_game_library(seat, profile, "steam")
            case "app":
                return await self._launch_app(seat, profile)
            case "custom":
                return await self._launch_custom(seat, profile)
            case _:
                log.warning("Unknown launcher type: %s", launcher)
                return None

    async def _launch_desktop(self, seat: "Seat", profile: "SeatProfile") -> asyncio.subprocess.Process | None:
        """Launch a desktop environment on the seat's display.

        On Linux: if an X/Wayland session is already running on this screen,
        do nothing. Otherwise start a lightweight session (openbox/xfce).
        On Windows: desktop is always present — nothing to launch.
        """
        system = platform.system()
        if system == "Windows":
            log.info("Seat %s: Windows desktop already present", seat.name)
            return None

        # Linux: check if a window manager is already running on this display
        display = seat.display.x_screen if seat.display else ":0"
        env = self._build_launch_env(
            GameInfo(id="desktop", name="Desktop", platform="", install_path="", source=""),
            seat,
        )

        # Try common lightweight WMs/DEs in order
        for wm in ["openbox-session", "xfce4-session", "startplasma-x11", "gnome-session", "i3"]:
            if _which(wm):
                log.info("Seat %s: launching %s on %s", seat.name, wm, display)
                return await self._spawn_session(wm, [], env, seat)

        log.warning("Seat %s: no desktop environment found", seat.name)
        return None

    async def _launch_game_library(self, seat: "Seat", profile: "SeatProfile",
                                    preferred: str) -> asyncio.subprocess.Process | None:
        """Launch a game library manager on the seat."""
        system = platform.system()
        env = self._build_launch_env(
            GameInfo(id="library", name="Game Library", platform="", install_path="", source=""),
            seat,
        )

        if preferred == "playnite" and system == "Windows":
            for path in [Path(os.environ.get("LOCALAPPDATA", "")) / "Playnite" / "Playnite.FullscreenApp.exe",
                         Path("C:/Program Files/Playnite/Playnite.FullscreenApp.exe"),
                         Path("C:/Program Files (x86)/Playnite/Playnite.FullscreenApp.exe")]:
                if path.exists():
                    return await self._spawn_session(str(path), [], env, seat)
            log.warning("Playnite not found, falling back to Steam")
            preferred = "steam"

        if preferred == "lutris" and system == "Linux":
            if _which("lutris"):
                return await self._spawn_session("lutris", [], env, seat)
            log.warning("Lutris not found, falling back to Steam")
            preferred = "steam"

        if preferred == "steam" or preferred == "playnite":
            # Steam Big Picture mode
            if system == "Linux" and _which("steam"):
                return await self._spawn_session("steam", ["-bigpicture"], env, seat)
            elif system == "Windows":
                steam_dir = _find_steam_dir()
                if steam_dir:
                    exe = steam_dir / "steam.exe"
                    if exe.exists():
                        return await self._spawn_session(str(exe), ["-bigpicture"], env, seat)

        log.warning("Seat %s: no game library found", seat.name)
        return None

    async def _launch_app(self, seat: "Seat", profile: "SeatProfile") -> asyncio.subprocess.Process | None:
        """Launch a single application on the seat."""
        cmd = profile.launcher_command
        if not cmd:
            log.error("Seat %s: app profile but no launcher_command set", seat.name)
            return None

        env = self._build_launch_env(
            GameInfo(id="app", name=cmd, platform="", install_path="", source=""),
            seat,
        )
        # Merge profile's extra env vars
        env.update(profile.launcher_env)

        args = list(profile.launcher_args)
        return await self._spawn_session(cmd, args, env, seat)

    async def _launch_custom(self, seat: "Seat", profile: "SeatProfile") -> asyncio.subprocess.Process | None:
        """Launch a custom command on the seat."""
        cmd = profile.launcher_command
        if not cmd:
            log.error("Seat %s: custom profile but no launcher_command set", seat.name)
            return None

        env = self._build_launch_env(
            GameInfo(id="custom", name=cmd, platform="", install_path="", source=""),
            seat,
        )
        env.update(profile.launcher_env)

        # Custom command may contain shell syntax
        log.info("Seat %s: launching custom: %s %s", seat.name, cmd, profile.launcher_args)
        try:
            if profile.launcher_args:
                proc = await asyncio.create_subprocess_exec(
                    cmd, *profile.launcher_args,
                    env=env,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.PIPE,
                    preexec_fn=os.setsid if platform.system() == "Linux" else None,
                )
            else:
                # Allow shell expansion for custom commands
                proc = await asyncio.create_subprocess_shell(
                    cmd,
                    env=env,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.PIPE,
                    preexec_fn=os.setsid if platform.system() == "Linux" else None,
                )
            self._running[seat.name] = _SeatGameState(
                game=GameInfo(id="custom", name=cmd, platform="custom",
                              install_path="", source="custom"),
                process=proc, pid=proc.pid,
                launched_at=__import__("time").time(),
            )
            asyncio.create_task(
                self._monitor_game(seat.name, proc),
                name=f"session-monitor-{seat.name}",
            )
            return proc
        except Exception as e:
            log.error("Failed to launch custom command on seat %s: %s", seat.name, e)
            return None

    async def _spawn_session(self, cmd: str, args: list[str], env: dict,
                              seat: "Seat") -> asyncio.subprocess.Process | None:
        """Common session spawn helper.  Uses seat isolation if configured."""
        full_cmd = [cmd] + args
        log.info("Seat %s: spawning %s", seat.name, " ".join(full_cmd))
        try:
            # Use isolation manager if the seat has an isolation context
            iso_mgr = self._seat_manager.isolation_manager
            iso_ctx = iso_mgr.get_context(seat.name)
            if iso_ctx and iso_ctx.backend_name != "none":
                proc = await iso_mgr.launch(seat.name, full_cmd, env)
            else:
                proc = await asyncio.create_subprocess_exec(
                    *full_cmd,
                    env=env,
                    stdout=asyncio.subprocess.DEVNULL,
                    stderr=asyncio.subprocess.PIPE,
                    preexec_fn=os.setsid if platform.system() == "Linux" else None,
                )
            import time
            self._running[seat.name] = _SeatGameState(
                game=GameInfo(id=cmd, name=cmd, platform="session",
                              install_path="", source="session"),
                process=proc, pid=proc.pid,
                launched_at=time.time(),
            )
            asyncio.create_task(
                self._monitor_game(seat.name, proc),
                name=f"session-monitor-{seat.name}",
            )
            return proc
        except Exception as e:
            log.error("Failed to spawn %s on seat %s: %s", cmd, seat.name, e)
            return None

    async def launch(self, game: GameInfo, seat: "Seat") -> asyncio.subprocess.Process | None:
        """
        Launch a game on a specific seat with correct display/audio/input env.

        Sets environment variables so the game renders on the seat's display
        and outputs audio to the seat's PipeWire sink. Notifies the encoder
        allocator to rebalance (game GPU should not encode).

        Returns the subprocess, or None on failure.
        """
        if seat.name in self._running:
            existing = self._running[seat.name]
            if existing.process and existing.process.returncode is None:
                log.warning("Seat %s already running %s — stopping first",
                            seat.name, existing.game.name)
                await self.stop(seat)

        env = self._build_launch_env(game, seat)
        cmd = self._build_launch_cmd(game)

        if not cmd:
            log.error("Cannot build launch command for %s (source=%s)",
                      game.name, game.source)
            return None

        log.info("Launching %s on seat %s: %s", game.name, seat.name, " ".join(cmd))

        try:
            proc = await asyncio.create_subprocess_exec(
                *cmd,
                env=env,
                stdout=asyncio.subprocess.DEVNULL,
                stderr=asyncio.subprocess.PIPE,
                preexec_fn=os.setsid if platform.system() == "Linux" else None,
            )

            import time
            state = _SeatGameState(
                game=game,
                process=proc,
                pid=proc.pid,
                launched_at=time.time(),
            )
            self._running[seat.name] = state

            # Notify encoder allocator: gaming GPU should not encode
            gpu_index = seat.display.index if seat.display else 0
            await self._seat_manager.rebalance_encoders(
                seat_name=seat.name, gaming_gpu_index=gpu_index,
            )

            # Monitor process exit in background
            asyncio.create_task(
                self._monitor_game(seat.name, proc),
                name=f"game-monitor-{seat.name}",
            )

            log.info("Game %s launched on seat %s (pid=%d)", game.name, seat.name, proc.pid)
            return proc

        except Exception as e:
            log.error("Failed to launch %s on seat %s: %s", game.name, seat.name, e)
            return None

    async def stop(self, seat: "Seat") -> bool:
        """
        Stop the game running on a seat.

        Sends SIGTERM, waits up to 10 seconds, then SIGKILL.
        Clears the encoder hint and triggers rebalance.
        """
        state = self._running.pop(seat.name, None)
        if not state:
            log.debug("No game running on seat %s", seat.name)
            return False

        proc = state.process
        if proc and proc.returncode is None:
            log.info("Stopping %s on seat %s (pid=%d)",
                     state.game.name, seat.name, state.pid)

            # Try graceful shutdown
            try:
                if platform.system() == "Linux":
                    # Kill the entire process group
                    os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                else:
                    proc.terminate()
            except (ProcessLookupError, OSError):
                pass

            try:
                await asyncio.wait_for(proc.wait(), timeout=10.0)
            except asyncio.TimeoutError:
                log.warning("Game %s on seat %s did not exit — sending SIGKILL",
                            state.game.name, seat.name)
                try:
                    if platform.system() == "Linux":
                        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                    else:
                        proc.kill()
                except (ProcessLookupError, OSError):
                    pass

                try:
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                except asyncio.TimeoutError:
                    log.error("Game %s on seat %s: SIGKILL failed, zombie process pid=%d",
                              state.game.name, seat.name, state.pid)

        # Clear encoder hint and rebalance
        await self._seat_manager.rebalance_encoders(
            seat_name=seat.name, gaming_gpu_index=None,
        )

        log.info("Game stopped on seat %s", seat.name)
        return True

    async def _monitor_game(self, seat_name: str, proc: asyncio.subprocess.Process) -> None:
        """Monitor a game process and clean up when it exits."""
        try:
            await proc.wait()
        except asyncio.CancelledError:
            return

        state = self._running.get(seat_name)
        if state and state.process is proc:
            log.info("Game %s on seat %s exited (code=%s)",
                     state.game.name, seat_name, proc.returncode)
            self._running.pop(seat_name, None)

            # Clear encoder hint and rebalance
            await self._seat_manager.rebalance_encoders(
                seat_name=seat_name, gaming_gpu_index=None,
            )

    def _build_launch_env(self, game: GameInfo, seat: "Seat") -> dict[str, str]:
        """Build the environment for launching a game on a specific seat."""
        env = dict(os.environ)
        system = platform.system()

        if system == "Linux":
            # Display — use the seat's X screen
            display = os.environ.get("DISPLAY", ":0")
            if seat.display and seat.display.x_screen:
                display = seat.display.x_screen
            env["DISPLAY"] = display

            # Audio — route to the seat's PipeWire null sink
            if seat.audio_sink:
                env["PULSE_SINK"] = seat.audio_sink
                env["SDL_AUDIODRIVER"] = "pulseaudio"

            # SDL fullscreen head — tells SDL which monitor to go fullscreen on
            env["SDL_VIDEO_FULLSCREEN_HEAD"] = str(seat.display_index)

            # Wayland — set output name if available
            if seat.display:
                env["SDL_VIDEO_WAYLAND_OUTPUT"] = seat.display.name

            # Vulkan device selection — avoid the encoding GPU
            # (game should render on the seat's display GPU)
            env.setdefault("DRI_PRIME", str(seat.display_index))

        elif system == "Windows":
            # On Windows, game placement is handled post-launch via
            # window management. Set audio endpoint if available.
            if seat.audio_sink:
                env["OZMA_AUDIO_ENDPOINT"] = seat.audio_sink

            # SDL display index
            env["SDL_VIDEO_FULLSCREEN_HEAD"] = str(seat.display_index)

        return env

    def _build_launch_cmd(self, game: GameInfo) -> list[str]:
        """Build the launch command for a game based on its source."""
        match game.source:
            case "steam":
                return self._cmd_steam(game)
            case "lutris":
                return self._cmd_lutris(game)
            case "playnite":
                return self._cmd_playnite(game)
            case _:
                # Try steam URI if platform is steam
                if game.platform == "steam":
                    return self._cmd_steam(game)
                log.warning("Unknown game source: %s", game.source)
                return []

    def _cmd_steam(self, game: GameInfo) -> list[str]:
        """Build Steam launch command."""
        system = platform.system()
        if system == "Linux":
            return ["steam", "-applaunch", game.id]
        elif system == "Windows":
            steam_dir = _find_steam_dir()
            if steam_dir:
                exe = steam_dir / "steam.exe"
                if exe.exists():
                    return [str(exe), "-applaunch", game.id]
            # Fall back to URI
            return ["cmd", "/c", "start", f"steam://rungameid/{game.id}"]
        return []

    def _cmd_lutris(self, game: GameInfo) -> list[str]:
        """Build Lutris launch command."""
        return ["lutris", f"lutris:rungame/{game.id}"]

    def _cmd_playnite(self, game: GameInfo) -> list[str]:
        """Build Playnite launch command."""
        system = platform.system()
        if system != "Windows":
            return []

        # Try Playnite URI
        return ["cmd", "/c", "start", f"playnite://playnite/start/{game.id}"]

    def to_dict(self) -> dict:
        """Serialize launcher state for diagnostics."""
        running = {}
        for seat_name, state in self._running.items():
            alive = state.process is not None and state.process.returncode is None
            running[seat_name] = {
                "game": state.game.name,
                "game_id": state.game.id,
                "pid": state.pid,
                "alive": alive,
            }
        return {
            "game_count": len(self._games),
            "running": running,
        }
