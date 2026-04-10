# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the routing intent system (Phase 2)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.model import MediaType
from routing.intent import (
    Constraints,
    Preferences,
    DegradationPolicy,
    StreamIntent,
    Intent,
    BUILTIN_INTENTS,
    compose_intents,
    EncryptionRequirement,
    VideoStrategy,
    AudioStrategy,
    HidStrategy,
)

pytestmark = pytest.mark.unit


# ── Constraints ────────────────────────────────────────────────────────────────

class TestConstraints:
    def test_defaults(self):
        c = Constraints()
        assert c.max_latency_ms is None
        assert c.max_loss is None
        assert c.encryption == EncryptionRequirement.none
        assert c.required_formats == []

    def test_to_dict_roundtrip(self):
        c = Constraints(
            max_latency_ms=50.0,
            max_hops=4,
            encryption=EncryptionRequirement.preferred,
        )
        d = c.to_dict()
        assert d["max_latency_ms"] == 50.0
        assert d["max_hops"] == 4
        assert d["encryption"] == "preferred"

    def test_intersect_picks_stricter_latency(self):
        a = Constraints(max_latency_ms=100.0)
        b = Constraints(max_latency_ms=50.0)
        r = a.intersect(b)
        assert r.max_latency_ms == 50.0

    def test_intersect_picks_none_when_one_is_none(self):
        a = Constraints(max_latency_ms=100.0)
        b = Constraints()
        r = a.intersect(b)
        assert r.max_latency_ms == 100.0

    def test_intersect_both_none(self):
        a = Constraints()
        b = Constraints()
        r = a.intersect(b)
        assert r.max_latency_ms is None

    def test_intersect_min_bandwidth_picks_higher(self):
        a = Constraints(min_bandwidth_bps=10_000_000)
        b = Constraints(min_bandwidth_bps=20_000_000)
        r = a.intersect(b)
        assert r.min_bandwidth_bps == 20_000_000

    def test_intersect_encryption_required_wins(self):
        a = Constraints(encryption=EncryptionRequirement.none)
        b = Constraints(encryption=EncryptionRequirement.required)
        r = a.intersect(b)
        assert r.encryption == EncryptionRequirement.required

    def test_intersect_encryption_preferred_over_none(self):
        a = Constraints(encryption=EncryptionRequirement.preferred)
        b = Constraints(encryption=EncryptionRequirement.none)
        r = a.intersect(b)
        assert r.encryption == EncryptionRequirement.preferred

    def test_intersect_required_formats_union(self):
        a = Constraints(required_formats=["h264"])
        b = Constraints(required_formats=["h265"])
        r = a.intersect(b)
        assert set(r.required_formats) == {"h264", "h265"}

    def test_intersect_forbidden_formats_union(self):
        a = Constraints(forbidden_formats=["mjpeg"])
        b = Constraints(forbidden_formats=["vp8"])
        r = a.intersect(b)
        assert set(r.forbidden_formats) == {"mjpeg", "vp8"}

    def test_intersect_hops_stricter(self):
        a = Constraints(max_hops=6)
        b = Constraints(max_hops=3)
        r = a.intersect(b)
        assert r.max_hops == 3


# ── Preferences ───────────────────────────────────────────────────────────────

class TestPreferences:
    def test_defaults(self):
        p = Preferences()
        assert not p.prefer_lossless
        assert not p.prefer_lower_latency
        assert p.target_latency_ms is None

    def test_to_dict(self):
        p = Preferences(target_resolution=(1920, 1080), prefer_lossless=True)
        d = p.to_dict()
        assert d["target_resolution"] == [1920, 1080]
        assert d["prefer_lossless"] is True

    def test_merge_other_overrides(self):
        a = Preferences(target_latency_ms=100.0, prefer_lossless=False)
        b = Preferences(target_latency_ms=50.0)
        r = a.merge(b)
        assert r.target_latency_ms == 50.0

    def test_merge_keeps_self_when_other_none(self):
        a = Preferences(target_framerate=60.0)
        b = Preferences()
        r = a.merge(b)
        assert r.target_framerate == 60.0

    def test_merge_bool_flags_or(self):
        a = Preferences(prefer_lossless=True, prefer_lower_latency=False)
        b = Preferences(prefer_lossless=False, prefer_lower_latency=True)
        r = a.merge(b)
        assert r.prefer_lossless is True
        assert r.prefer_lower_latency is True

    def test_merge_resolution(self):
        a = Preferences(target_resolution=(1280, 720))
        b = Preferences(target_resolution=(3840, 2160))
        r = a.merge(b)
        assert r.target_resolution == (3840, 2160)


# ── DegradationPolicy ─────────────────────────────────────────────────────────

class TestDegradationPolicy:
    def test_defaults(self):
        d = DegradationPolicy()
        assert d.video == VideoStrategy.reduce_framerate_first
        assert d.audio == AudioStrategy.allow_lossy_compression
        assert d.hid == HidStrategy.never_degrade

    def test_to_dict(self):
        d = DegradationPolicy(
            video=VideoStrategy.never_degrade,
            audio=AudioStrategy.never_degrade,
            hid=HidStrategy.never_degrade,
        )
        r = d.to_dict()
        assert r["video"] == "never_degrade"
        assert r["audio"] == "never_degrade"
        assert r["hid"] == "never_degrade"


# ── StreamIntent ──────────────────────────────────────────────────────────────

class TestStreamIntent:
    def test_basic(self):
        si = StreamIntent(media_type=MediaType.hid, required=True)
        assert si.media_type == MediaType.hid
        assert si.required

    def test_to_dict(self):
        si = StreamIntent(
            media_type=MediaType.video,
            required=True,
            constraints=Constraints(max_latency_ms=100.0),
        )
        d = si.to_dict()
        assert d["media_type"] == "video"
        assert d["required"] is True
        assert d["constraints"]["max_latency_ms"] == 100.0


# ── Built-in intents ──────────────────────────────────────────────────────────

class TestBuiltinIntents:
    def test_all_eight_exist(self):
        expected = {
            "control", "preview", "observe", "desktop",
            "creative", "gaming", "broadcast", "fidelity_audio",
            "secure",
        }
        assert set(BUILTIN_INTENTS.keys()) == expected

    def test_control_has_hid_stream(self):
        intent = BUILTIN_INTENTS["control"]
        si = intent.stream_for(MediaType.hid)
        assert si is not None
        assert si.required

    def test_desktop_has_hid_video_audio(self):
        intent = BUILTIN_INTENTS["desktop"]
        assert intent.stream_for(MediaType.hid) is not None
        assert intent.stream_for(MediaType.video) is not None
        assert intent.stream_for(MediaType.audio) is not None

    def test_gaming_hid_latency_tight(self):
        gaming = BUILTIN_INTENTS["gaming"]
        hid = gaming.stream_for(MediaType.hid)
        assert hid.constraints.max_latency_ms <= 5.0

    def test_fidelity_audio_lossless(self):
        intent = BUILTIN_INTENTS["fidelity_audio"]
        audio = intent.stream_for(MediaType.audio)
        assert audio.preferences.prefer_lossless

    def test_creative_never_degrade(self):
        intent = BUILTIN_INTENTS["creative"]
        assert intent.degradation.video == VideoStrategy.never_degrade
        assert intent.degradation.audio == AudioStrategy.never_degrade
        assert intent.degradation.hid == HidStrategy.never_degrade

    def test_gaming_priority_higher_than_desktop(self):
        assert BUILTIN_INTENTS["gaming"].priority > BUILTIN_INTENTS["desktop"].priority

    def test_control_hid_zero_loss(self):
        intent = BUILTIN_INTENTS["control"]
        hid = intent.stream_for(MediaType.hid)
        assert hid.constraints.max_loss == 0.0

    def test_all_have_descriptions(self):
        for name, intent in BUILTIN_INTENTS.items():
            assert intent.description, f"{name} has no description"

    def test_to_dict_roundtrip(self):
        for name, intent in BUILTIN_INTENTS.items():
            d = intent.to_dict()
            assert d["name"] == name
            assert isinstance(d["streams"], list)

    def test_stream_for_missing_returns_none(self):
        intent = BUILTIN_INTENTS["control"]
        assert intent.stream_for(MediaType.video) is None


# ── compose_intents ────────────────────────────────────────────────────────────

class TestComposeIntents:
    def test_single_intent_passthrough(self):
        i = BUILTIN_INTENTS["control"]
        assert compose_intents([i]) is i

    def test_empty_raises(self):
        with pytest.raises(ValueError):
            compose_intents([])

    def test_composed_name(self):
        i1 = BUILTIN_INTENTS["control"]
        i2 = BUILTIN_INTENTS["preview"]
        result = compose_intents([i1, i2])
        assert "control" in result.name
        assert "preview" in result.name

    def test_composed_priority_is_max(self):
        i1 = BUILTIN_INTENTS["preview"]   # priority 20
        i2 = BUILTIN_INTENTS["gaming"]    # priority 70
        result = compose_intents([i1, i2])
        assert result.priority == 70

    def test_composed_streams_union(self):
        i1 = BUILTIN_INTENTS["control"]   # only hid
        i2 = BUILTIN_INTENTS["preview"]   # only video
        result = compose_intents([i1, i2])
        types = {s.media_type for s in result.streams}
        assert MediaType.hid in types
        assert MediaType.video in types

    def test_composed_same_stream_intersects_constraints(self):
        i1 = Intent(
            name="a",
            streams=[StreamIntent(
                media_type=MediaType.hid,
                constraints=Constraints(max_latency_ms=30.0),
            )],
        )
        i2 = Intent(
            name="b",
            streams=[StreamIntent(
                media_type=MediaType.hid,
                constraints=Constraints(max_latency_ms=10.0),
            )],
        )
        result = compose_intents([i1, i2])
        hid = result.stream_for(MediaType.hid)
        assert hid.constraints.max_latency_ms == 10.0

    def test_composed_degradation_most_restrictive(self):
        i1 = Intent(
            name="a",
            streams=[],
            degradation=DegradationPolicy(
                video=VideoStrategy.allow_lossy_compression,
                audio=AudioStrategy.never_degrade,
                hid=HidStrategy.never_degrade,
            ),
        )
        i2 = Intent(
            name="b",
            streams=[],
            degradation=DegradationPolicy(
                video=VideoStrategy.never_degrade,
                audio=AudioStrategy.reduce_sample_rate,
                hid=HidStrategy.drop_stream,
            ),
        )
        result = compose_intents([i1, i2])
        # never_degrade is most restrictive for video
        assert result.degradation.video == VideoStrategy.never_degrade
        # never_degrade > reduce_sample_rate for audio
        assert result.degradation.audio == AudioStrategy.never_degrade
        # never_degrade > drop_stream for hid
        assert result.degradation.hid == HidStrategy.never_degrade
