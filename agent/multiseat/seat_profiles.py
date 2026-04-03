# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Seat profiles for multi-seat.

A profile defines what runs on a seat (launcher), how it's captured
(quality/latency), and audio settings. The launcher determines what
the user sees — a full desktop, a game library, a single app, or
a custom command.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass(frozen=True)
class SeatProfile:
    """Configuration profile for a seat."""
    name: str
    description: str

    # ── Launcher ──────────────────────────────────────────────
    # What runs on this seat's display.
    launcher: str             # "desktop", "playnite", "lutris", "steam", "app", "custom"
    launcher_command: str = ""  # for "app"/"custom": the command to run
    launcher_args: list[str] = field(default_factory=list)
    launcher_fullscreen: bool = True
    launcher_env: dict[str, str] = field(default_factory=dict)  # extra env vars

    # ── Capture ───────────────────────────────────────────────
    capture_fps: int = 30
    capture_width: int = 1920
    capture_height: int = 1080
    capture_crf: int = 25           # x264 CRF (lower = better quality)
    capture_preset: str = "ultrafast"
    low_latency: bool = True        # tune=zerolatency

    # ── Audio ─────────────────────────────────────────────────
    audio_channels: int = 2         # 2 = stereo, 6 = 5.1, 8 = 7.1
    audio_sample_rate: int = 48000

    # ── Isolation ────────────────────────────────────────────
    # Per-seat process isolation backend.
    # "none"         No isolation (display/input separation only)
    # "user"         Separate Windows user per seat (best, solves single-instance)
    # "sandboxie"    Sandboxie-Plus sandbox per seat (good, no user accounts)
    # "appcontainer" Windows AppContainer (lightest, built-in, some games fail)
    # "namespace"    Linux PID/mount namespaces (unshare)
    isolation: str = "none"

    def to_dict(self) -> dict:
        d = {
            "name": self.name,
            "description": self.description,
            "launcher": self.launcher,
            "capture_fps": self.capture_fps,
            "capture_resolution": f"{self.capture_width}x{self.capture_height}",
            "low_latency": self.low_latency,
            "audio_channels": self.audio_channels,
        }
        if self.launcher_command:
            d["launcher_command"] = self.launcher_command
        if self.launcher_args:
            d["launcher_args"] = self.launcher_args
        if self.launcher_env:
            d["launcher_env"] = self.launcher_env
        if self.isolation != "none":
            d["isolation"] = self.isolation
        return d


# ── Built-in profiles ────────────────────────────────────────────────────

DESKTOP = SeatProfile(
    name="desktop",
    description="Full desktop session — user gets a complete desktop environment",
    launcher="desktop",
    capture_fps=30,
    capture_crf=25,
    low_latency=True,
)

GAMING = SeatProfile(
    name="gaming",
    description="Game library (Playnite/Lutris/Steam) — low-latency, 60fps",
    launcher="playnite",  # auto-detects: playnite → lutris → steam
    capture_fps=60,
    capture_crf=23,
    capture_preset="ultrafast",
    low_latency=True,
)

WORKSTATION = SeatProfile(
    name="workstation",
    description="Standard workstation — balanced quality, lower bandwidth",
    launcher="desktop",
    capture_fps=15,
    capture_crf=28,
    low_latency=False,
)

MEDIA = SeatProfile(
    name="media",
    description="Media player (Kodi/Jellyfin/Plex) — 30fps, surround audio",
    launcher="app",
    launcher_command="kodi",
    capture_fps=30,
    capture_crf=25,
    capture_preset="fast",
    audio_channels=6,
    low_latency=False,
)

KIOSK = SeatProfile(
    name="kiosk",
    description="Single app in fullscreen — locked-down, minimal resources",
    launcher="app",
    launcher_command="",  # set via seat config
    capture_fps=10,
    capture_width=1280,
    capture_height=720,
    capture_crf=30,
    low_latency=False,
)

SIGNAGE = SeatProfile(
    name="signage",
    description="Digital signage — browser fullscreen to a URL, minimal interaction",
    launcher="app",
    launcher_command="chromium",
    launcher_args=["--kiosk", "--noerrdialogs", "--disable-infobars"],
    launcher_fullscreen=True,
    capture_fps=10,
    capture_width=1920,
    capture_height=1080,
    capture_crf=30,
    low_latency=False,
)

CUSTOM = SeatProfile(
    name="custom",
    description="Custom command — you specify what runs",
    launcher="custom",
    capture_fps=30,
    capture_crf=25,
    low_latency=True,
)

PROFILES: dict[str, SeatProfile] = {
    "desktop": DESKTOP,
    "gaming": GAMING,
    "workstation": WORKSTATION,
    "media": MEDIA,
    "kiosk": KIOSK,
    "signage": SIGNAGE,
    "custom": CUSTOM,
}


def get_profile(name: str) -> SeatProfile:
    """Get a profile by name. Falls back to desktop if not found."""
    return PROFILES.get(name, DESKTOP)
