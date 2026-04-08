# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Intent system for the Ozma routing graph.

Implements the intent data model from docs/routing/intents.md.
Intents express *what* a user wants from the routing system — the router
turns intents into concrete pipeline recommendations.

Phase 2: intent definitions + composition. Router in router.py.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from .model import MediaType


# ── Enums ─────────────────────────────────────────────────────────────────────

class EncryptionRequirement(str, Enum):
    required = "required"
    preferred = "preferred"
    none = "none"


class VideoStrategy(str, Enum):
    never_degrade = "never_degrade"
    reduce_framerate_first = "reduce_framerate_first"
    reduce_resolution_first = "reduce_resolution_first"
    allow_lossy_compression = "allow_lossy_compression"
    drop_stream = "drop_stream"


class AudioStrategy(str, Enum):
    never_degrade = "never_degrade"
    allow_lossy_compression = "allow_lossy_compression"
    reduce_sample_rate = "reduce_sample_rate"
    increase_latency = "increase_latency"
    drop_stream = "drop_stream"


class HidStrategy(str, Enum):
    never_degrade = "never_degrade"
    increase_latency = "increase_latency"
    drop_stream = "drop_stream"


# ── Constraints ───────────────────────────────────────────────────────────────

@dataclass
class Constraints:
    """Hard limits — any candidate pipeline that violates these is rejected."""
    max_latency_ms: float | None = None
    max_activation_time_ms: float | None = None
    min_bandwidth_bps: int | None = None
    max_loss: float | None = None          # 0.0–1.0
    max_jitter_ms: float | None = None
    max_hops: int | None = None
    max_conversions: int | None = None
    required_formats: list[str] = field(default_factory=list)
    forbidden_formats: list[str] = field(default_factory=list)
    encryption: EncryptionRequirement = EncryptionRequirement.none

    def to_dict(self) -> dict:
        return {
            "max_latency_ms": self.max_latency_ms,
            "max_activation_time_ms": self.max_activation_time_ms,
            "min_bandwidth_bps": self.min_bandwidth_bps,
            "max_loss": self.max_loss,
            "max_jitter_ms": self.max_jitter_ms,
            "max_hops": self.max_hops,
            "max_conversions": self.max_conversions,
            "required_formats": list(self.required_formats),
            "forbidden_formats": list(self.forbidden_formats),
            "encryption": self.encryption.value,
        }

    def intersect(self, other: "Constraints") -> "Constraints":
        """
        Return the *stricter* intersection of two Constraints.

        For numeric limits, the lower value wins. For lists, union of both
        (required from either is still required; forbidden from either is still
        forbidden). For encryption, the stricter requirement wins.
        """
        def _min(a, b):
            if a is None: return b
            if b is None: return a
            return min(a, b)

        def _max(a, b):
            if a is None: return b
            if b is None: return a
            return max(a, b)

        # encryption: required > preferred > none
        _enc_rank = {
            EncryptionRequirement.required: 2,
            EncryptionRequirement.preferred: 1,
            EncryptionRequirement.none: 0,
        }
        enc = max([self.encryption, other.encryption], key=lambda e: _enc_rank[e])

        return Constraints(
            max_latency_ms=_min(self.max_latency_ms, other.max_latency_ms),
            max_activation_time_ms=_min(self.max_activation_time_ms, other.max_activation_time_ms),
            min_bandwidth_bps=_max(self.min_bandwidth_bps, other.min_bandwidth_bps),
            max_loss=_min(self.max_loss, other.max_loss),
            max_jitter_ms=_min(self.max_jitter_ms, other.max_jitter_ms),
            max_hops=_min(self.max_hops, other.max_hops),
            max_conversions=_min(self.max_conversions, other.max_conversions),
            required_formats=list(set(self.required_formats) | set(other.required_formats)),
            forbidden_formats=list(set(self.forbidden_formats) | set(other.forbidden_formats)),
            encryption=enc,
        )


# ── Preferences ───────────────────────────────────────────────────────────────

@dataclass
class Preferences:
    """Soft targets — influence pipeline ranking but don't eliminate candidates."""
    target_latency_ms: float | None = None
    target_resolution: tuple[int, int] | None = None   # (width, height)
    target_framerate: float | None = None
    target_sample_rate: int | None = None
    target_channels: int | None = None
    target_bit_depth: int | None = None
    prefer_lossless: bool = False
    prefer_hardware_codec: bool = False
    prefer_fewer_hops: bool = False
    prefer_lower_latency: bool = False
    prefer_higher_quality: bool = False

    def to_dict(self) -> dict:
        return {
            "target_latency_ms": self.target_latency_ms,
            "target_resolution": list(self.target_resolution) if self.target_resolution else None,
            "target_framerate": self.target_framerate,
            "target_sample_rate": self.target_sample_rate,
            "target_channels": self.target_channels,
            "target_bit_depth": self.target_bit_depth,
            "prefer_lossless": self.prefer_lossless,
            "prefer_hardware_codec": self.prefer_hardware_codec,
            "prefer_fewer_hops": self.prefer_fewer_hops,
            "prefer_lower_latency": self.prefer_lower_latency,
            "prefer_higher_quality": self.prefer_higher_quality,
        }

    def merge(self, other: "Preferences") -> "Preferences":
        """Merge two Preferences; *other* overrides non-None fields."""
        def _pick(a, b):
            return b if b is not None else a

        return Preferences(
            target_latency_ms=_pick(self.target_latency_ms, other.target_latency_ms),
            target_resolution=_pick(self.target_resolution, other.target_resolution),
            target_framerate=_pick(self.target_framerate, other.target_framerate),
            target_sample_rate=_pick(self.target_sample_rate, other.target_sample_rate),
            target_channels=_pick(self.target_channels, other.target_channels),
            target_bit_depth=_pick(self.target_bit_depth, other.target_bit_depth),
            prefer_lossless=other.prefer_lossless or self.prefer_lossless,
            prefer_hardware_codec=other.prefer_hardware_codec or self.prefer_hardware_codec,
            prefer_fewer_hops=other.prefer_fewer_hops or self.prefer_fewer_hops,
            prefer_lower_latency=other.prefer_lower_latency or self.prefer_lower_latency,
            prefer_higher_quality=other.prefer_higher_quality or self.prefer_higher_quality,
        )


# ── Degradation policy ────────────────────────────────────────────────────────

@dataclass
class DegradationPolicy:
    """Per-media-type strategy when the preferred pipeline cannot be satisfied."""
    video: VideoStrategy = VideoStrategy.reduce_framerate_first
    audio: AudioStrategy = AudioStrategy.allow_lossy_compression
    hid: HidStrategy = HidStrategy.never_degrade

    def to_dict(self) -> dict:
        return {
            "video": self.video.value,
            "audio": self.audio.value,
            "hid": self.hid.value,
        }


# ── StreamIntent ──────────────────────────────────────────────────────────────

@dataclass
class StreamIntent:
    """Requirements and preferences for one media stream within an intent."""
    media_type: MediaType
    required: bool = True
    constraints: Constraints = field(default_factory=Constraints)
    preferences: Preferences = field(default_factory=Preferences)

    def to_dict(self) -> dict:
        return {
            "media_type": self.media_type.value,
            "required": self.required,
            "constraints": self.constraints.to_dict(),
            "preferences": self.preferences.to_dict(),
        }


# ── Intent ────────────────────────────────────────────────────────────────────

@dataclass
class Intent:
    """
    A named bundle of requirements and preferences for a routing session.

    The router resolves an active intent against the RoutingGraph to produce
    a ranked list of candidate Pipelines.
    """
    name: str
    description: str = ""
    streams: list[StreamIntent] = field(default_factory=list)
    priority: int = 50              # 0–100, higher wins on tie
    degradation: DegradationPolicy = field(default_factory=DegradationPolicy)

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "description": self.description,
            "streams": [s.to_dict() for s in self.streams],
            "priority": self.priority,
            "degradation": self.degradation.to_dict(),
        }

    def stream_for(self, media_type: MediaType) -> StreamIntent | None:
        """Return the StreamIntent for the given media type, or None."""
        for s in self.streams:
            if s.media_type == media_type:
                return s
        return None


# ── Built-in intents ──────────────────────────────────────────────────────────

BUILTIN_INTENTS: dict[str, Intent] = {

    "control": Intent(
        name="control",
        description="HID input only — no video or audio required.",
        priority=80,
        streams=[
            StreamIntent(
                media_type=MediaType.hid,
                required=True,
                constraints=Constraints(
                    max_latency_ms=20.0,
                    max_activation_time_ms=100.0,
                    max_loss=0.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    prefer_lower_latency=True,
                    prefer_fewer_hops=True,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            hid=HidStrategy.never_degrade,
        ),
    ),

    "preview": Intent(
        name="preview",
        description="Low-quality video thumbnail — monitoring only.",
        priority=20,
        streams=[
            StreamIntent(
                media_type=MediaType.video,
                required=True,
                constraints=Constraints(
                    max_latency_ms=500.0,
                    max_hops=6,
                    max_conversions=2,
                ),
                preferences=Preferences(
                    target_resolution=(320, 240),
                    target_framerate=5.0,
                    prefer_fewer_hops=True,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            video=VideoStrategy.reduce_resolution_first,
        ),
    ),

    "observe": Intent(
        name="observe",
        description="Watch-only video + audio, no HID.",
        priority=30,
        streams=[
            StreamIntent(
                media_type=MediaType.video,
                required=True,
                constraints=Constraints(
                    max_latency_ms=2000.0,
                    max_hops=6,
                    max_conversions=2,
                ),
                preferences=Preferences(
                    target_resolution=(1920, 1080),
                    target_framerate=30.0,
                    prefer_higher_quality=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.audio,
                required=False,
                constraints=Constraints(
                    max_latency_ms=2000.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    target_sample_rate=48000,
                    target_channels=2,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            video=VideoStrategy.reduce_framerate_first,
            audio=AudioStrategy.allow_lossy_compression,
        ),
    ),

    "desktop": Intent(
        name="desktop",
        description="Standard desktop KVM — video + audio + HID.",
        priority=50,
        streams=[
            StreamIntent(
                media_type=MediaType.hid,
                required=True,
                constraints=Constraints(
                    max_latency_ms=30.0,
                    max_loss=0.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    prefer_lower_latency=True,
                    prefer_fewer_hops=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.video,
                required=True,
                constraints=Constraints(
                    max_latency_ms=150.0,
                    max_hops=6,
                    max_conversions=2,
                ),
                preferences=Preferences(
                    target_resolution=(1920, 1080),
                    target_framerate=60.0,
                    prefer_lower_latency=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.audio,
                required=False,
                constraints=Constraints(
                    max_latency_ms=150.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    target_sample_rate=48000,
                    target_channels=2,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            video=VideoStrategy.reduce_framerate_first,
            audio=AudioStrategy.allow_lossy_compression,
            hid=HidStrategy.never_degrade,
        ),
    ),

    "creative": Intent(
        name="creative",
        description="High-quality video + lossless audio — creative work.",
        priority=60,
        streams=[
            StreamIntent(
                media_type=MediaType.hid,
                required=True,
                constraints=Constraints(
                    max_latency_ms=20.0,
                    max_loss=0.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    prefer_lower_latency=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.video,
                required=True,
                constraints=Constraints(
                    max_latency_ms=100.0,
                    max_hops=4,
                    max_conversions=1,
                ),
                preferences=Preferences(
                    target_resolution=(3840, 2160),
                    target_framerate=60.0,
                    prefer_lossless=True,
                    prefer_higher_quality=True,
                    prefer_hardware_codec=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.audio,
                required=True,
                constraints=Constraints(
                    max_latency_ms=100.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    target_sample_rate=96000,
                    target_channels=2,
                    target_bit_depth=24,
                    prefer_lossless=True,
                    prefer_higher_quality=True,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            video=VideoStrategy.never_degrade,
            audio=AudioStrategy.never_degrade,
            hid=HidStrategy.never_degrade,
        ),
    ),

    "gaming": Intent(
        name="gaming",
        description="Low-latency video + audio + HID for gaming.",
        priority=70,
        streams=[
            StreamIntent(
                media_type=MediaType.hid,
                required=True,
                constraints=Constraints(
                    max_latency_ms=5.0,
                    max_loss=0.0,
                    max_hops=3,
                ),
                preferences=Preferences(
                    prefer_lower_latency=True,
                    prefer_fewer_hops=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.video,
                required=True,
                constraints=Constraints(
                    max_latency_ms=50.0,
                    max_hops=4,
                    max_conversions=1,
                ),
                preferences=Preferences(
                    target_resolution=(1920, 1080),
                    target_framerate=144.0,
                    prefer_lower_latency=True,
                    prefer_hardware_codec=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.audio,
                required=False,
                constraints=Constraints(
                    max_latency_ms=50.0,
                    max_hops=4,
                ),
                preferences=Preferences(
                    target_sample_rate=48000,
                    target_channels=2,
                    prefer_lower_latency=True,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            video=VideoStrategy.reduce_resolution_first,
            audio=AudioStrategy.allow_lossy_compression,
            hid=HidStrategy.never_degrade,
        ),
    ),

    "broadcast": Intent(
        name="broadcast",
        description="High-quality outbound stream for recording or streaming.",
        priority=40,
        streams=[
            StreamIntent(
                media_type=MediaType.video,
                required=True,
                constraints=Constraints(
                    max_hops=6,
                    max_conversions=2,
                ),
                preferences=Preferences(
                    target_resolution=(1920, 1080),
                    target_framerate=60.0,
                    prefer_higher_quality=True,
                    prefer_hardware_codec=True,
                ),
            ),
            StreamIntent(
                media_type=MediaType.audio,
                required=True,
                constraints=Constraints(
                    max_hops=4,
                ),
                preferences=Preferences(
                    target_sample_rate=48000,
                    target_channels=2,
                    target_bit_depth=16,
                    prefer_higher_quality=True,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            video=VideoStrategy.reduce_framerate_first,
            audio=AudioStrategy.allow_lossy_compression,
        ),
    ),

    "fidelity_audio": Intent(
        name="fidelity_audio",
        description="High-fidelity audio only — lossless, low jitter.",
        priority=65,
        streams=[
            StreamIntent(
                media_type=MediaType.audio,
                required=True,
                constraints=Constraints(
                    max_latency_ms=20.0,
                    max_jitter_ms=0.1,
                    max_loss=0.0,
                    max_hops=4,
                    max_conversions=1,
                ),
                preferences=Preferences(
                    target_sample_rate=192000,
                    target_channels=2,
                    target_bit_depth=32,
                    prefer_lossless=True,
                    prefer_higher_quality=True,
                ),
            ),
        ],
        degradation=DegradationPolicy(
            audio=AudioStrategy.never_degrade,
        ),
    ),
}


# ── Intent composition ────────────────────────────────────────────────────────

def compose_intents(intents: list[Intent]) -> Intent:
    """
    Merge multiple intents into one composite intent.

    Rules:
    - Name: joined with "+"
    - Priority: maximum of all priorities
    - Streams: union of all stream intents; same media_type → constraints
      intersected (stricter), preferences merged (later overrides earlier)
    - Degradation: most restrictive strategy per media type
    """
    if not intents:
        raise ValueError("compose_intents requires at least one intent")
    if len(intents) == 1:
        return intents[0]

    name = "+".join(i.name for i in intents)
    priority = max(i.priority for i in intents)

    # Merge streams: collect all unique media types
    stream_map: dict[MediaType, StreamIntent] = {}
    for intent in intents:
        for si in intent.streams:
            if si.media_type not in stream_map:
                stream_map[si.media_type] = StreamIntent(
                    media_type=si.media_type,
                    required=si.required,
                    constraints=si.constraints,
                    preferences=si.preferences,
                )
            else:
                existing = stream_map[si.media_type]
                stream_map[si.media_type] = StreamIntent(
                    media_type=si.media_type,
                    required=existing.required or si.required,
                    constraints=existing.constraints.intersect(si.constraints),
                    preferences=existing.preferences.merge(si.preferences),
                )

    # Degradation: most restrictive per media type
    _vid_rank = {s: i for i, s in enumerate([
        VideoStrategy.allow_lossy_compression,
        VideoStrategy.reduce_resolution_first,
        VideoStrategy.reduce_framerate_first,
        VideoStrategy.drop_stream,
        VideoStrategy.never_degrade,
    ])}
    _aud_rank = {s: i for i, s in enumerate([
        AudioStrategy.drop_stream,
        AudioStrategy.increase_latency,
        AudioStrategy.reduce_sample_rate,
        AudioStrategy.allow_lossy_compression,
        AudioStrategy.never_degrade,
    ])}
    _hid_rank = {s: i for i, s in enumerate([
        HidStrategy.drop_stream,
        HidStrategy.increase_latency,
        HidStrategy.never_degrade,
    ])}

    best_deg = DegradationPolicy()
    for intent in intents:
        d = intent.degradation
        if _vid_rank[d.video] > _vid_rank[best_deg.video]:
            best_deg.video = d.video
        if _aud_rank[d.audio] > _aud_rank[best_deg.audio]:
            best_deg.audio = d.audio
        if _hid_rank[d.hid] > _hid_rank[best_deg.hid]:
            best_deg.hid = d.hid

    return Intent(
        name=name,
        description=f"Composed from: {name}",
        streams=list(stream_map.values()),
        priority=priority,
        degradation=best_deg,
    )
