# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Seat profiles for multi-seat.

Profiles define capture quality, audio, and latency settings per seat
based on intended usage.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class SeatProfile:
    """Configuration profile for a seat."""
    name: str
    capture_fps: int
    capture_width: int
    capture_height: int
    capture_crf: int          # x264 CRF (lower = better quality)
    capture_preset: str       # x264 preset
    audio_channels: int       # 2 = stereo, 6 = 5.1, 8 = 7.1
    audio_sample_rate: int
    low_latency: bool         # tune=zerolatency for x264
    description: str

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "capture_fps": self.capture_fps,
            "capture_resolution": f"{self.capture_width}x{self.capture_height}",
            "capture_crf": self.capture_crf,
            "audio_channels": self.audio_channels,
            "audio_sample_rate": self.audio_sample_rate,
            "low_latency": self.low_latency,
            "description": self.description,
        }


GAMING = SeatProfile(
    name="gaming",
    capture_fps=60,
    capture_width=1920,
    capture_height=1080,
    capture_crf=23,
    capture_preset="ultrafast",
    audio_channels=2,
    audio_sample_rate=48000,
    low_latency=True,
    description="Low-latency gaming: 60fps, fast encode, stereo audio",
)

WORKSTATION = SeatProfile(
    name="workstation",
    capture_fps=15,
    capture_width=1920,
    capture_height=1080,
    capture_crf=28,
    capture_preset="ultrafast",
    audio_channels=2,
    audio_sample_rate=48000,
    low_latency=False,
    description="Standard workstation: 15fps, balanced quality",
)

MEDIA = SeatProfile(
    name="media",
    capture_fps=30,
    capture_width=1920,
    capture_height=1080,
    capture_crf=25,
    capture_preset="fast",
    audio_channels=6,
    audio_sample_rate=48000,
    low_latency=False,
    description="Media consumption: 30fps, surround audio",
)

KIOSK = SeatProfile(
    name="kiosk",
    capture_fps=10,
    capture_width=1280,
    capture_height=720,
    capture_crf=30,
    capture_preset="ultrafast",
    audio_channels=2,
    audio_sample_rate=44100,
    low_latency=False,
    description="Kiosk/signage: low bandwidth, minimal resources",
)

PROFILES: dict[str, SeatProfile] = {
    "gaming": GAMING,
    "workstation": WORKSTATION,
    "media": MEDIA,
    "kiosk": KIOSK,
}


def get_profile(name: str) -> SeatProfile:
    """Get a profile by name. Falls back to workstation if not found."""
    return PROFILES.get(name, WORKSTATION)
