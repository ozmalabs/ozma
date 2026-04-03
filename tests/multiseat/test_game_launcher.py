# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Tests for agent.multiseat.game_launcher — game discovery and launch."""

from __future__ import annotations

import json
import sqlite3
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from agent.multiseat.game_launcher import (
    GameInfo, GameLauncher, _parse_vdf, _vdf_tokenize,
    _discover_steam_games, _discover_lutris_games,
    _playnite_plugin_to_source,
)


# ── VDF parser ───────────────────────────────────────────────────────────────

class TestVDFParser:
    def test_simple_key_value(self):
        vdf = '"key" "value"'
        result = _parse_vdf(vdf)
        assert result["key"] == "value"

    def test_nested_section(self):
        vdf = '''
"AppState"
{
    "appid"     "570"
    "name"      "Dota 2"
    "installdir"    "dota 2 beta"
}
'''
        result = _parse_vdf(vdf)
        assert "AppState" in result
        assert result["AppState"]["appid"] == "570"
        assert result["AppState"]["name"] == "Dota 2"
        assert result["AppState"]["installdir"] == "dota 2 beta"

    def test_libraryfolders_format(self):
        vdf = '''
"libraryfolders"
{
    "0"
    {
        "path"      "/home/user/.steam/steam"
        "label"     ""
        "apps"
        {
            "570"   "12345678"
        }
    }
    "1"
    {
        "path"      "/mnt/games/Steam"
        "label"     "Games"
    }
}
'''
        result = _parse_vdf(vdf)
        assert "libraryfolders" in result
        folders = result["libraryfolders"]
        assert folders["0"]["path"] == "/home/user/.steam/steam"
        assert folders["1"]["path"] == "/mnt/games/Steam"

    def test_comments_ignored(self):
        vdf = '''
// This is a comment
"key" "value"
// Another comment
"key2" "value2"
'''
        result = _parse_vdf(vdf)
        assert result["key"] == "value"
        assert result["key2"] == "value2"

    def test_empty_input(self):
        result = _parse_vdf("")
        assert result == {}

    def test_unquoted_tokens(self):
        vdf = 'key value'
        result = _parse_vdf(vdf)
        assert result["key"] == "value"

    def test_escape_in_value(self):
        vdf = r'"path" "C:\\Program Files\\Steam"'
        result = _parse_vdf(vdf)
        assert "path" in result

    def test_real_appmanifest(self):
        """Parse a realistic appmanifest_*.acf file."""
        vdf = '''
"AppState"
{
    "appid"     "730"
    "Universe"      "1"
    "name"      "Counter-Strike 2"
    "StateFlags"        "4"
    "installdir"        "Counter-Strike Global Offensive"
    "LastUpdated"       "1700000000"
    "SizeOnDisk"        "35000000000"
    "buildid"       "12345678"
    "LastOwner"     "76561198000000000"
    "UpdateResult"      "0"
    "BytesToDownload"       "0"
    "BytesDownloaded"       "0"
    "BytesToStage"      "0"
    "BytesStaged"       "0"
    "AutoUpdateBehavior"        "0"
    "AllowOtherDownloadsWhileRunning"       "0"
    "ScheduledAutoUpdate"       "0"
}
'''
        result = _parse_vdf(vdf)
        app = result["AppState"]
        assert app["appid"] == "730"
        assert app["name"] == "Counter-Strike 2"
        assert app["installdir"] == "Counter-Strike Global Offensive"


class TestVDFTokenizer:
    def test_tokenize_basic(self):
        tokens = _vdf_tokenize('"key" "value"')
        assert tokens == ["key", "value"]

    def test_tokenize_braces(self):
        tokens = _vdf_tokenize('"section" { "key" "val" }')
        assert tokens == ["section", "{", "key", "val", "}"]

    def test_tokenize_comments(self):
        tokens = _vdf_tokenize('// comment\n"key" "val"')
        assert tokens == ["key", "val"]

    def test_tokenize_whitespace(self):
        tokens = _vdf_tokenize('  "key"  \t  "val"  \n  ')
        assert tokens == ["key", "val"]


# ── Steam discovery ──────────────────────────────────────────────────────────

class TestSteamDiscovery:
    def test_discover_from_appmanifests(self, tmp_path):
        """Discover games from appmanifest_*.acf files."""
        steamapps = tmp_path / "steamapps"
        steamapps.mkdir()

        # Create appmanifest files
        (steamapps / "appmanifest_730.acf").write_text('''
"AppState"
{
    "appid"     "730"
    "name"      "Counter-Strike 2"
    "installdir"        "Counter-Strike Global Offensive"
}
''')
        (steamapps / "appmanifest_570.acf").write_text('''
"AppState"
{
    "appid"     "570"
    "name"      "Dota 2"
    "installdir"        "dota 2 beta"
}
''')

        games = _discover_steam_games(tmp_path)
        assert len(games) == 2
        names = {g.name for g in games}
        assert "Counter-Strike 2" in names
        assert "Dota 2" in names

    def test_discover_skips_invalid_manifests(self, tmp_path):
        steamapps = tmp_path / "steamapps"
        steamapps.mkdir()

        # Valid manifest
        (steamapps / "appmanifest_730.acf").write_text('''
"AppState"
{
    "appid"     "730"
    "name"      "Counter-Strike 2"
    "installdir"        "csgo"
}
''')
        # Invalid manifest (no appid)
        (steamapps / "appmanifest_000.acf").write_text('''
"AppState"
{
    "name"      ""
}
''')

        games = _discover_steam_games(tmp_path)
        assert len(games) == 1
        assert games[0].name == "Counter-Strike 2"

    def test_discover_multiple_libraries(self, tmp_path):
        """Discover games from multiple Steam library folders."""
        # Main library
        steamapps = tmp_path / "steamapps"
        steamapps.mkdir()
        (steamapps / "appmanifest_730.acf").write_text('''
"AppState" { "appid" "730" "name" "CS2" "installdir" "csgo" }
''')

        # libraryfolders.vdf with second library
        second_lib = tmp_path / "games"
        second_steamapps = second_lib / "steamapps"
        second_steamapps.mkdir(parents=True)
        (second_steamapps / "appmanifest_570.acf").write_text('''
"AppState" { "appid" "570" "name" "Dota 2" "installdir" "dota" }
''')

        (steamapps / "libraryfolders.vdf").write_text(f'''
"libraryfolders"
{{
    "0"
    {{
        "path"      "{tmp_path}"
    }}
    "1"
    {{
        "path"      "{second_lib}"
    }}
}}
''')

        games = _discover_steam_games(tmp_path)
        names = {g.name for g in games}
        assert "CS2" in names
        assert "Dota 2" in names

    def test_discover_empty_library(self, tmp_path):
        steamapps = tmp_path / "steamapps"
        steamapps.mkdir()
        games = _discover_steam_games(tmp_path)
        assert games == []

    def test_game_install_path(self, tmp_path):
        steamapps = tmp_path / "steamapps"
        steamapps.mkdir()
        (steamapps / "appmanifest_730.acf").write_text('''
"AppState" { "appid" "730" "name" "CS2" "installdir" "csgo" }
''')

        games = _discover_steam_games(tmp_path)
        assert games[0].install_path == str(steamapps / "common" / "csgo")
        assert games[0].platform == "steam"
        assert games[0].source == "steam"


# ── Lutris discovery ─────────────────────────────────────────────────────────

class TestLutrisDiscovery:
    def test_discover_from_sqlite(self, tmp_path):
        """Discover games from Lutris pga.db."""
        db_path = tmp_path / "pga.db"
        conn = sqlite3.connect(str(db_path))
        conn.execute("""
            CREATE TABLE games (
                slug TEXT, name TEXT, runner TEXT,
                directory TEXT, installed INTEGER, playtime REAL
            )
        """)
        conn.execute(
            "INSERT INTO games VALUES (?, ?, ?, ?, ?, ?)",
            ("doom-eternal", "Doom Eternal", "wine", "/games/doom", 1, 5.5),
        )
        conn.execute(
            "INSERT INTO games VALUES (?, ?, ?, ?, ?, ?)",
            ("not-installed", "Not Installed", "wine", "/games/ni", 0, 0),
        )
        conn.commit()
        conn.close()

        with patch("agent.multiseat.game_launcher.Path") as MockPath:
            mock_home = tmp_path
            MockPath.home.return_value = mock_home
            # Create the expected directory structure
            db_dir = mock_home / ".local" / "share" / "lutris"
            db_dir.mkdir(parents=True, exist_ok=True)
            import shutil
            shutil.copy(str(db_path), str(db_dir / "pga.db"))

            with patch("agent.multiseat.game_launcher.platform") as mock_platform:
                mock_platform.system.return_value = "Linux"
                with patch("agent.multiseat.game_launcher.Path.home",
                           return_value=mock_home):
                    games = _discover_lutris_games()

        assert len(games) == 1
        assert games[0].name == "Doom Eternal"
        assert games[0].id == "doom-eternal"
        assert games[0].source == "lutris"


# ── Playnite plugin mapping ─────────────────────────────────────────────────

class TestPlaynitePluginMapping:
    def test_steam_plugin(self):
        assert _playnite_plugin_to_source("cb91dfc9-b977-43bf-8e70-55f46e410fab") == "steam"

    def test_gog_plugin(self):
        assert _playnite_plugin_to_source("aebe8b7c-6dc3-4a66-af31-e7375c6b5e9e") == "gog"

    def test_epic_plugin(self):
        assert _playnite_plugin_to_source("00000002-dbd1-46c6-b5d0-b1ba559d10e4") == "epic"

    def test_unknown_plugin(self):
        assert _playnite_plugin_to_source("unknown-guid") == ""


# ── GameInfo data model ──────────────────────────────────────────────────────

class TestGameInfo:
    def test_basic_game(self):
        game = GameInfo(
            id="730", name="Counter-Strike 2",
            platform="steam", install_path="/games/csgo",
        )
        assert game.id == "730"
        assert game.name == "Counter-Strike 2"
        assert game.source == ""

    def test_to_dict(self):
        game = GameInfo(
            id="730", name="CS2", platform="steam",
            install_path="/games/csgo", source="steam",
            playtime_minutes=120,
        )
        d = game.to_dict()
        assert d["id"] == "730"
        assert d["name"] == "CS2"
        assert d["platform"] == "steam"
        assert d["playtime_minutes"] == 120


# ── Launch environment ───────────────────────────────────────────────────────

class TestLaunchEnvironment:
    def test_linux_env_vars(self):
        """Launch env should set DISPLAY, PULSE_SINK, SDL vars."""
        mock_manager = MagicMock()
        launcher = GameLauncher(mock_manager)

        game = GameInfo(id="730", name="CS2", platform="steam",
                        install_path="/games/csgo", source="steam")

        mock_seat = MagicMock()
        mock_seat.display = MagicMock()
        mock_seat.display.x_screen = ":0.1"
        mock_seat.display.name = "HDMI-2"
        mock_seat.display_index = 1
        mock_seat.audio_sink = "ozma-seat-1"
        mock_seat.name = "seat-1"

        with patch("agent.multiseat.game_launcher.platform") as mock_platform:
            mock_platform.system.return_value = "Linux"
            env = launcher._build_launch_env(game, mock_seat)

        assert env["DISPLAY"] == ":0.1"
        assert env["PULSE_SINK"] == "ozma-seat-1"
        assert env["SDL_VIDEO_FULLSCREEN_HEAD"] == "1"
        assert env["SDL_VIDEO_WAYLAND_OUTPUT"] == "HDMI-2"

    def test_env_without_audio(self):
        mock_manager = MagicMock()
        launcher = GameLauncher(mock_manager)

        game = GameInfo(id="1", name="Test", platform="test",
                        install_path="/test", source="steam")

        mock_seat = MagicMock()
        mock_seat.display = None
        mock_seat.display_index = 0
        mock_seat.audio_sink = None
        mock_seat.name = "seat-0"

        with patch("agent.multiseat.game_launcher.platform") as mock_platform:
            mock_platform.system.return_value = "Linux"
            env = launcher._build_launch_env(game, mock_seat)

        assert "PULSE_SINK" not in env


# ── Launch command construction ──────────────────────────────────────────────

class TestLaunchCommand:
    def test_steam_linux_cmd(self):
        mock_manager = MagicMock()
        launcher = GameLauncher(mock_manager)

        game = GameInfo(id="730", name="CS2", platform="steam",
                        install_path="/games/csgo", source="steam")

        with patch("agent.multiseat.game_launcher.platform") as mock_platform:
            mock_platform.system.return_value = "Linux"
            cmd = launcher._build_launch_cmd(game)

        assert cmd == ["steam", "-applaunch", "730"]

    def test_lutris_cmd(self):
        mock_manager = MagicMock()
        launcher = GameLauncher(mock_manager)

        game = GameInfo(id="doom-eternal", name="Doom Eternal",
                        platform="wine", install_path="/games/doom",
                        source="lutris")
        cmd = launcher._build_launch_cmd(game)
        assert cmd == ["lutris", "lutris:rungame/doom-eternal"]

    def test_unknown_source_empty(self):
        mock_manager = MagicMock()
        launcher = GameLauncher(mock_manager)

        game = GameInfo(id="x", name="Unknown", platform="unknown",
                        install_path="/x", source="unknown")
        cmd = launcher._build_launch_cmd(game)
        assert cmd == []

    def test_steam_fallback_for_steam_platform(self):
        """If source is unknown but platform is steam, use steam command."""
        mock_manager = MagicMock()
        launcher = GameLauncher(mock_manager)

        game = GameInfo(id="730", name="CS2", platform="steam",
                        install_path="/games/csgo", source="other")

        with patch("agent.multiseat.game_launcher.platform") as mock_platform:
            mock_platform.system.return_value = "Linux"
            cmd = launcher._build_launch_cmd(game)

        assert cmd == ["steam", "-applaunch", "730"]


# ── GameLauncher state ───────────────────────────────────────────────────────

class TestGameLauncherState:
    def test_running_game_none(self):
        launcher = GameLauncher(MagicMock())
        assert launcher.running_game("seat-0") is None

    def test_running_state_none(self):
        launcher = GameLauncher(MagicMock())
        assert launcher.running_state("seat-0") is None

    def test_to_dict_empty(self):
        launcher = GameLauncher(MagicMock())
        d = launcher.to_dict()
        assert d["game_count"] == 0
        assert d["running"] == {}

    def test_games_property(self):
        launcher = GameLauncher(MagicMock())
        launcher._games = [
            GameInfo(id="1", name="Game 1", platform="steam",
                     install_path="/g1", source="steam"),
        ]
        assert len(launcher.games) == 1
        # Should return a copy
        launcher.games.append(GameInfo(id="2", name="Game 2",
                                       platform="steam", install_path="/g2"))
        assert len(launcher._games) == 1  # original unchanged
