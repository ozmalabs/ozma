# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Unit tests for the format system and format negotiation (Phase 3)."""

from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent.parent.parent.parent / "controller"))

import pytest
from routing.model import MediaType
from routing.formats import (
    AudioFormat,
    ControlFormat,
    ControlInputSet,
    DataFormat,
    DataField,
    DataSchema,
    Format,
    FormatRange,
    FormatSet,
    HidFormat,
    NegotiationFailure,
    NumericRange,
    RgbFormat,
    ScreenFormat,
    VideoFormat,
    audio_bandwidth_uncompressed,
    enumerate_formats,
    fixate_format,
    hid_bandwidth,
    negotiate_format,
    restrict_formats,
    rgb_bandwidth,
    screen_bandwidth_raw,
    serial_bandwidth_bytes_per_sec,
    video_bandwidth_uncompressed,
)

pytestmark = pytest.mark.unit


# ── NumericRange ──────────────────────────────────────────────────────────────

class TestNumericRange:
    def test_contains(self):
        r = NumericRange(10, 100)
        assert r.contains(50)
        assert r.contains(10)
        assert r.contains(100)
        assert not r.contains(9)
        assert not r.contains(101)

    def test_intersect_overlap(self):
        a = NumericRange(10, 100)
        b = NumericRange(50, 200)
        result = a.intersect(b)
        assert result is not None
        assert result.min == 50
        assert result.max == 100

    def test_intersect_no_overlap(self):
        a = NumericRange(10, 20)
        b = NumericRange(30, 40)
        assert a.intersect(b) is None

    def test_intersect_adjacent(self):
        a = NumericRange(10, 30)
        b = NumericRange(30, 50)
        result = a.intersect(b)
        assert result is not None
        assert result.min == 30
        assert result.max == 30

    def test_to_dict(self):
        r = NumericRange(1, 60)
        d = r.to_dict()
        assert d["min"] == 1
        assert d["max"] == 60


# ── VideoFormat ───────────────────────────────────────────────────────────────

class TestVideoFormat:
    def test_uncompressed_bandwidth(self):
        # 1920x1080 @ 30fps, 8-bit, 3 channels
        vf = VideoFormat(codec="raw", width=1920, height=1080,
                         framerate=30.0, bit_depth=8)
        expected = 1920 * 1080 * 8 * 3 * 30
        assert vf.bandwidth_bps == expected

    def test_compressed_uses_bitrate(self):
        vf = VideoFormat(codec="h264", bitrate_bps=8_000_000)
        assert vf.bandwidth_bps == 8_000_000

    def test_to_dict(self):
        vf = VideoFormat(codec="h264", width=1920, height=1080, framerate=60.0)
        d = vf.to_dict()
        assert d["codec"] == "h264"
        assert d["width"] == 1920
        assert d["framerate"] == 60.0

    def test_lossy_flag(self):
        assert not VideoFormat(codec="raw").lossy
        vf = VideoFormat(codec="h264", lossy=True)
        assert vf.lossy


# ── AudioFormat ───────────────────────────────────────────────────────────────

class TestAudioFormat:
    def test_pcm_bandwidth(self):
        # 48kHz stereo 16-bit
        af = AudioFormat(codec="pcm", sample_rate=48000, channels=2, bit_depth=16)
        assert af.bandwidth_bps == 48000 * 2 * 16

    def test_compressed_uses_bitrate(self):
        af = AudioFormat(codec="opus", bitrate_bps=128_000)
        assert af.bandwidth_bps == 128_000

    def test_to_dict(self):
        af = AudioFormat(codec="pcm", sample_rate=96000, channels=6)
        d = af.to_dict()
        assert d["codec"] == "pcm"
        assert d["sample_rate"] == 96000
        assert d["channels"] == 6


# ── HidFormat ─────────────────────────────────────────────────────────────────

class TestHidFormat:
    def test_bandwidth_keyboard(self):
        # 1000 Hz, 8 bytes
        hf = HidFormat(device_type="keyboard", report_rate_hz=1000, report_size_bytes=8)
        assert hf.bandwidth_bps == 64_000

    def test_bandwidth_mouse(self):
        hf = HidFormat(device_type="mouse", report_rate_hz=1000, report_size_bytes=6)
        assert hf.bandwidth_bps == 48_000

    def test_to_dict(self):
        hf = HidFormat()
        d = hf.to_dict()
        assert d["device_type"] == "keyboard"
        assert d["protocol"] == "report"


# ── RgbFormat ─────────────────────────────────────────────────────────────────

class TestRgbFormat:
    def test_bandwidth(self):
        rf = RgbFormat(led_count=300, framerate=30.0, color_depth=8)
        assert rf.bandwidth_bps == 300 * 3 * 8 * 30

    def test_white_channel_adds_4th(self):
        rf = RgbFormat(led_count=100, framerate=30.0, color_depth=8, white_channel=True)
        assert rf.bandwidth_bps == 100 * 4 * 8 * 30

    def test_to_dict(self):
        rf = RgbFormat(encoding="ddp", led_count=150)
        d = rf.to_dict()
        assert d["encoding"] == "ddp"
        assert d["led_count"] == 150


# ── Format ────────────────────────────────────────────────────────────────────

class TestFormat:
    def test_video_bandwidth_delegated(self):
        f = Format(
            media_type=MediaType.video,
            video=VideoFormat(codec="h264", bitrate_bps=5_000_000),
        )
        assert f.bandwidth_bps == 5_000_000

    def test_audio_bandwidth_delegated(self):
        f = Format(
            media_type=MediaType.audio,
            audio=AudioFormat(codec="pcm", sample_rate=48000, channels=2, bit_depth=16),
        )
        assert f.bandwidth_bps == 48000 * 2 * 16

    def test_to_dict_video(self):
        f = Format(
            media_type=MediaType.video,
            video=VideoFormat(codec="h265", width=3840, height=2160, framerate=60.0),
        )
        d = f.to_dict()
        assert d["media_type"] == "video"
        assert d["video"]["codec"] == "h265"

    def test_to_dict_audio(self):
        f = Format(
            media_type=MediaType.audio,
            audio=AudioFormat(codec="opus"),
        )
        d = f.to_dict()
        assert d["media_type"] == "audio"


# ── FormatRange ───────────────────────────────────────────────────────────────

class TestFormatRange:
    def test_video_intersect_common_codec(self):
        a = FormatRange(media_type=MediaType.video, video_codecs=["h264", "h265"])
        b = FormatRange(media_type=MediaType.video, video_codecs=["h265", "av1"])
        r = a.intersect(b)
        assert r is not None
        assert r.video_codecs == ["h265"]

    def test_video_intersect_no_common_codec(self):
        a = FormatRange(media_type=MediaType.video, video_codecs=["h264"])
        b = FormatRange(media_type=MediaType.video, video_codecs=["h265"])
        assert a.intersect(b) is None

    def test_audio_intersect_sample_rates(self):
        a = FormatRange(media_type=MediaType.audio,
                        audio_codecs=["pcm"],
                        audio_sample_rates=[44100, 48000])
        b = FormatRange(media_type=MediaType.audio,
                        audio_codecs=["pcm"],
                        audio_sample_rates=[48000, 96000])
        r = a.intersect(b)
        assert r is not None
        assert r.audio_sample_rates == [48000]

    def test_hid_intersect_protocols(self):
        a = FormatRange(media_type=MediaType.hid,
                        hid_device_types=["keyboard"],
                        hid_protocols=["boot", "report"])
        b = FormatRange(media_type=MediaType.hid,
                        hid_device_types=["keyboard"],
                        hid_protocols=["report"])
        r = a.intersect(b)
        assert r is not None
        assert "report" in r.hid_protocols

    def test_different_media_type_returns_none(self):
        a = FormatRange(media_type=MediaType.video, video_codecs=["h264"])
        b = FormatRange(media_type=MediaType.audio, audio_codecs=["pcm"])
        assert a.intersect(b) is None

    def test_can_produce_video(self):
        r = FormatRange(
            media_type=MediaType.video,
            video_codecs=["h264", "h265"],
            video_framerate=NumericRange(1, 60),
        )
        f = Format(media_type=MediaType.video,
                   video=VideoFormat(codec="h264", framerate=30.0))
        assert r.can_produce(f)

    def test_can_produce_video_codec_mismatch(self):
        r = FormatRange(media_type=MediaType.video, video_codecs=["h264"])
        f = Format(media_type=MediaType.video,
                   video=VideoFormat(codec="av1", framerate=30.0))
        assert not r.can_produce(f)


# ── FormatSet ─────────────────────────────────────────────────────────────────

class TestFormatSet:
    def _video_set(self, codecs):
        return FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=codecs)
        ])

    def test_intersect_common(self):
        a = self._video_set(["h264", "h265"])
        b = self._video_set(["h265", "av1"])
        r = a.intersect(b)
        assert not r.is_empty()
        vr = r.for_media_type(MediaType.video)
        assert vr is not None
        assert vr.video_codecs == ["h265"]

    def test_intersect_no_overlap(self):
        a = self._video_set(["h264"])
        b = self._video_set(["h265"])
        r = a.intersect(b)
        assert r.is_empty()

    def test_intersect_drops_missing_media_type(self):
        a = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["h264"]),
            FormatRange(media_type=MediaType.audio, audio_codecs=["pcm"]),
        ])
        b = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["h264"]),
        ])
        r = a.intersect(b)
        assert r.for_media_type(MediaType.video) is not None
        assert r.for_media_type(MediaType.audio) is None

    def test_empty(self):
        assert FormatSet().is_empty()
        assert not FormatSet(formats=[
            FormatRange(media_type=MediaType.hid)
        ]).is_empty()


# ── Negotiation phases ────────────────────────────────────────────────────────

class TestEnumerateFormats:
    def test_single_set_passthrough(self):
        fs = FormatSet(formats=[
            FormatRange(media_type=MediaType.hid, hid_device_types=["keyboard"])
        ])
        result = enumerate_formats([fs])
        assert not result.is_empty()

    def test_empty_list(self):
        result = enumerate_formats([])
        assert result.is_empty()

    def test_two_sets_intersected(self):
        a = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["h264", "h265"])
        ])
        b = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["h265", "av1"])
        ])
        result = enumerate_formats([a, b])
        vr = result.for_media_type(MediaType.video)
        assert vr is not None
        assert "h265" in vr.video_codecs
        assert "h264" not in vr.video_codecs


class TestRestrictFormats:
    def test_no_constraints_passthrough(self):
        fs = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["h264", "h265"])
        ])
        result = restrict_formats(fs)
        vr = result.for_media_type(MediaType.video)
        assert len(vr.video_codecs) == 2

    def test_required_format_filters(self):
        fs = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["h264", "h265", "av1"])
        ])
        result = restrict_formats(fs, required_formats=["h265"])
        vr = result.for_media_type(MediaType.video)
        assert vr.video_codecs == ["h265"]

    def test_forbidden_format_excluded(self):
        fs = FormatSet(formats=[
            FormatRange(media_type=MediaType.audio,
                        audio_codecs=["pcm", "opus", "aac"])
        ])
        result = restrict_formats(fs, forbidden_formats=["aac"])
        ar = result.for_media_type(MediaType.audio)
        assert "aac" not in ar.audio_codecs
        assert "pcm" in ar.audio_codecs

    def test_all_forbidden_removes_entry(self):
        fs = FormatSet(formats=[
            FormatRange(media_type=MediaType.video, video_codecs=["mjpeg"])
        ])
        result = restrict_formats(fs, forbidden_formats=["mjpeg"])
        assert result.for_media_type(MediaType.video) is None


class TestFixateFormat:
    def _video_set(self, codecs, w=(1, 3840), h=(1, 2160), fps=(1, 144)):
        return FormatSet(formats=[FormatRange(
            media_type=MediaType.video,
            video_codecs=codecs,
            video_resolution=(NumericRange(*w), NumericRange(*h)),
            video_framerate=NumericRange(*fps),
        )])

    def _audio_set(self, codecs=None, rates=None, depths=None):
        return FormatSet(formats=[FormatRange(
            media_type=MediaType.audio,
            audio_codecs=codecs or ["pcm"],
            audio_sample_rates=rates or [48000],
            audio_bit_depths=depths or [16],
        )])

    def test_video_basic(self):
        fs = self._video_set(["h264", "h265"])
        fmt = fixate_format(fs, MediaType.video)
        assert fmt.media_type == MediaType.video
        assert fmt.video is not None

    def test_video_prefers_lossless(self):
        fs = self._video_set(["raw", "h264"])
        fmt = fixate_format(fs, MediaType.video, prefer_lossless=True)
        assert fmt.video.codec == "raw"

    def test_video_prefer_hardware_codec(self):
        fs = self._video_set(["vp9", "h264", "mjpeg"])
        fmt = fixate_format(fs, MediaType.video, prefer_hardware_codec=True)
        assert fmt.video.codec == "h264"  # h264 ranks before vp9 and mjpeg

    def test_video_target_resolution(self):
        fs = self._video_set(["h264"])
        fmt = fixate_format(fs, MediaType.video, target_resolution=(1280, 720))
        assert fmt.video.width == 1280
        assert fmt.video.height == 720

    def test_video_target_framerate(self):
        fs = self._video_set(["h264"])
        fmt = fixate_format(fs, MediaType.video, target_framerate=144.0)
        assert fmt.video.framerate == 144.0

    def test_audio_basic(self):
        fs = self._audio_set()
        fmt = fixate_format(fs, MediaType.audio)
        assert fmt.audio is not None
        assert fmt.audio.codec == "pcm"

    def test_audio_prefers_lossless(self):
        fs = self._audio_set(codecs=["opus", "flac", "pcm"])
        fmt = fixate_format(fs, MediaType.audio, prefer_lossless=True)
        assert fmt.audio.codec in ("pcm", "flac")

    def test_audio_target_sample_rate(self):
        fs = self._audio_set(rates=[44100, 48000, 96000])
        fmt = fixate_format(fs, MediaType.audio, target_sample_rate=96000)
        assert fmt.audio.sample_rate == 96000

    def test_audio_fallback_if_target_not_available(self):
        fs = self._audio_set(rates=[44100, 48000])
        # 192000 not available, should pick highest available
        fmt = fixate_format(fs, MediaType.audio, target_sample_rate=192000)
        assert fmt.audio.sample_rate in (44100, 48000)

    def test_hid_basic(self):
        fs = FormatSet(formats=[FormatRange(
            media_type=MediaType.hid,
            hid_device_types=["keyboard", "mouse"],
            hid_protocols=["report"],
        )])
        fmt = fixate_format(fs, MediaType.hid)
        assert fmt.hid is not None

    def test_missing_media_type_raises(self):
        fs = FormatSet(formats=[])
        with pytest.raises(NegotiationFailure):
            fixate_format(fs, MediaType.video)


# ── negotiate_format (end-to-end) ─────────────────────────────────────────────

class TestNegotiateFormat:
    def test_video_end_to_end(self):
        source = FormatSet(formats=[FormatRange(
            media_type=MediaType.video,
            video_codecs=["h264", "h265", "raw"],
            video_framerate=NumericRange(1, 144),
        )])
        sink = FormatSet(formats=[FormatRange(
            media_type=MediaType.video,
            video_codecs=["h264", "av1"],
        )])
        fmt = negotiate_format([source, sink], MediaType.video,
                               target_resolution=(1920, 1080),
                               target_framerate=60.0)
        assert fmt.video.codec == "h264"
        assert fmt.video.width == 1920
        assert fmt.video.framerate == 60.0

    def test_no_common_format_raises(self):
        source = FormatSet(formats=[FormatRange(
            media_type=MediaType.video, video_codecs=["h264"]
        )])
        sink = FormatSet(formats=[FormatRange(
            media_type=MediaType.video, video_codecs=["h265"]
        )])
        with pytest.raises(NegotiationFailure):
            negotiate_format([source, sink], MediaType.video)

    def test_forbidden_format_excluded(self):
        source = FormatSet(formats=[FormatRange(
            media_type=MediaType.audio, audio_codecs=["pcm", "aac"]
        )])
        sink = FormatSet(formats=[FormatRange(
            media_type=MediaType.audio, audio_codecs=["pcm", "aac"]
        )])
        fmt = negotiate_format([source, sink], MediaType.audio,
                               forbidden_formats=["aac"])
        assert fmt.audio.codec == "pcm"

    def test_required_format_selected(self):
        source = FormatSet(formats=[FormatRange(
            media_type=MediaType.audio, audio_codecs=["pcm", "opus", "aac"]
        )])
        sink = FormatSet(formats=[FormatRange(
            media_type=MediaType.audio, audio_codecs=["pcm", "opus", "aac"]
        )])
        fmt = negotiate_format([source, sink], MediaType.audio,
                               required_formats=["opus"])
        assert fmt.audio.codec == "opus"

    def test_hid_negotiation(self):
        source = FormatSet(formats=[FormatRange(
            media_type=MediaType.hid,
            hid_device_types=["keyboard", "mouse"],
            hid_protocols=["boot", "report"],
        )])
        sink = FormatSet(formats=[FormatRange(
            media_type=MediaType.hid,
            hid_device_types=["keyboard"],
            hid_protocols=["report"],
        )])
        fmt = negotiate_format([source, sink], MediaType.hid)
        assert fmt.hid.device_type == "keyboard"
        assert fmt.hid.protocol == "report"


# ── Bandwidth helpers ─────────────────────────────────────────────────────────

class TestBandwidthHelpers:
    def test_video_uncompressed_1080p30(self):
        bw = video_bandwidth_uncompressed(1920, 1080, 8, 30.0, 3)
        assert bw == 1920 * 1080 * 8 * 3 * 30

    def test_audio_uncompressed_48k_stereo_16(self):
        bw = audio_bandwidth_uncompressed(48000, 2, 16)
        assert bw == 48000 * 2 * 16

    def test_hid_bw_keyboard_1000hz(self):
        bw = hid_bandwidth(1000, 8)
        assert bw == 64_000

    def test_screen_raw_streamdeck(self):
        # Stream Deck XL: 480x384 @ 15fps, 24-bit
        bw = screen_bandwidth_raw(480, 384, 24, 15)
        assert bw == 480 * 384 * 24 * 15

    def test_rgb_strip_300led(self):
        bw = rgb_bandwidth(300, color_depth=8, framerate=30.0)
        assert bw == 300 * 3 * 8 * 30

    def test_rgb_rgbw_adds_channel(self):
        bw_rgb = rgb_bandwidth(100, 8, 30.0, white_channel=False)
        bw_rgbw = rgb_bandwidth(100, 8, 30.0, white_channel=True)
        assert bw_rgbw == bw_rgb * 4 // 3

    def test_serial_115200_8n1(self):
        bps = serial_bandwidth_bytes_per_sec(115200, 8, "none", 1.0)
        # 115200 / (1 + 8 + 0 + 1) = 11520 bytes/sec
        assert bps == 11520

    def test_serial_dmx512(self):
        bps = serial_bandwidth_bytes_per_sec(250000, 8, "none", 2.0)
        # 250000 / 11 ≈ 22727 bytes/sec
        assert bps == 22727

    def test_serial_9600(self):
        bps = serial_bandwidth_bytes_per_sec(9600)
        assert bps == 960
