# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Format system for the Ozma routing graph — Phase 3.

Implements the full format hierarchy from docs/routing/formats.md:
  VideoFormat, AudioFormat, HidFormat, ScreenFormat, RgbFormat,
  ControlFormat, DataFormat, FormatSet, FormatRange.

Also implements the three-phase format negotiation algorithm:
  1. Enumerate  — each port reports its FormatSet
  2. Restrict   — compute intersection; insert converters if empty
  3. Fixate     — select one concrete Format per link

Bandwidth calculation helpers are included for all media types.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from .model import MediaType


# ── Video ─────────────────────────────────────────────────────────────────────

@dataclass
class VideoFormat:
    codec: str = "raw"                # "raw", "h264", "h265", "av1", "vp9", "mjpeg", "ndi"
    container: str | None = None      # "rtp", "rtsp", "hls", "mpegts", "raw"
    width: int = 0
    height: int = 0
    framerate: float = 0.0
    color_space: str | None = None    # "bt709", "bt2020", "srgb"
    bit_depth: int = 8
    hdr: bool = False
    chroma_subsampling: str | None = None  # "4:4:4", "4:2:2", "4:2:0"
    bitrate_bps: int | None = None
    profile: str | None = None
    level: str | None = None
    keyframe_interval: int | None = None
    lossy: bool = False

    @property
    def bandwidth_bps(self) -> int:
        """Estimated bandwidth in bits per second."""
        if self.bitrate_bps is not None:
            return self.bitrate_bps
        if self.width and self.height and self.framerate:
            # Uncompressed: width * height * bit_depth * channels(1 luma + 2 chroma) * fps
            channels = 3
            return int(self.width * self.height * self.bit_depth * channels * self.framerate)
        return 0

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "codec": self.codec,
            "width": self.width,
            "height": self.height,
            "framerate": self.framerate,
            "bit_depth": self.bit_depth,
            "hdr": self.hdr,
            "lossy": self.lossy,
        }
        for attr in ("container", "color_space", "chroma_subsampling", "bitrate_bps",
                     "profile", "level", "keyframe_interval"):
            v = getattr(self, attr)
            if v is not None:
                d[attr] = v
        return d


# ── Audio ─────────────────────────────────────────────────────────────────────

@dataclass
class AudioFormat:
    codec: str = "pcm"               # "pcm", "opus", "aac", "flac", "vban", "aes67"
    container: str | None = None
    sample_rate: int = 48000
    channels: int = 2
    bit_depth: int = 16
    sample_format: str | None = None  # "int", "float"
    bitrate_bps: int | None = None
    frame_size: int | None = None     # samples per frame
    lossy: bool = False
    channel_layout: str | None = None  # "stereo", "5.1", "7.1"
    channel_map: list[str] = field(default_factory=list)  # ["FL","FR","FC","LFE","SL","SR"]

    @property
    def bandwidth_bps(self) -> int:
        if self.bitrate_bps is not None:
            return self.bitrate_bps
        return self.sample_rate * self.channels * self.bit_depth

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "codec": self.codec,
            "sample_rate": self.sample_rate,
            "channels": self.channels,
            "bit_depth": self.bit_depth,
            "lossy": self.lossy,
        }
        for attr in ("container", "sample_format", "bitrate_bps", "frame_size",
                     "channel_layout"):
            v = getattr(self, attr)
            if v is not None:
                d[attr] = v
        if self.channel_map:
            d["channel_map"] = list(self.channel_map)
        return d


# ── HID ──────────────────────────────────────────────────────────────────────

@dataclass
class HidFormat:
    device_type: str = "keyboard"     # "keyboard","mouse","gamepad","tablet","consumer"
    report_rate_hz: int = 125
    report_size_bytes: int = 8
    protocol: str = "report"          # "boot", "report", "ozma-extended"
    absolute_positioning: bool = False

    @property
    def bandwidth_bps(self) -> int:
        return self.report_rate_hz * self.report_size_bytes * 8

    def to_dict(self) -> dict:
        return {
            "device_type": self.device_type,
            "report_rate_hz": self.report_rate_hz,
            "report_size_bytes": self.report_size_bytes,
            "protocol": self.protocol,
            "absolute_positioning": self.absolute_positioning,
        }


# ── Screen ────────────────────────────────────────────────────────────────────

@dataclass
class DataField:
    key: str
    type: str = "string"              # "string","number","bool","enum","timestamp","list","object"
    unit: str | None = None
    enum_values: list[str] = field(default_factory=list)
    description: str = ""
    required: bool = False
    default: Any = None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "key": self.key,
            "type": self.type,
            "required": self.required,
        }
        if self.unit:
            d["unit"] = self.unit
        if self.enum_values:
            d["enum_values"] = list(self.enum_values)
        if self.description:
            d["description"] = self.description
        if self.default is not None:
            d["default"] = self.default
        return d


@dataclass
class DataSchema:
    fields: list[DataField] = field(default_factory=list)
    update_mode: str = "push"         # "event", "poll", "push"
    max_update_rate_hz: float | None = None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "fields": [f.to_dict() for f in self.fields],
            "update_mode": self.update_mode,
        }
        if self.max_update_rate_hz is not None:
            d["max_update_rate_hz"] = self.max_update_rate_hz
        return d


@dataclass
class ScreenFormat:
    encoding: str = "raw_rgb"         # "raw_rgb","raw_rgb565","jpeg","png","widget_def","typed_data"
    width: int | None = None
    height: int | None = None
    framerate: float | None = None
    color_depth: int | None = None    # 16 or 24 bpp
    color_space: str | None = None
    rotation: int = 0                 # 0, 90, 180, 270
    dithering: bool = False
    partial_update: bool = False
    rendering_tier: int = 0           # 0=push raw, 1=server-render, 2=native widget, 3=data-driven
    data_schema: DataSchema | None = None

    @property
    def bandwidth_bps(self) -> int:
        if self.width and self.height and self.color_depth and self.framerate:
            return int(self.width * self.height * self.color_depth * self.framerate)
        return 0

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "encoding": self.encoding,
            "rotation": self.rotation,
            "dithering": self.dithering,
            "partial_update": self.partial_update,
            "rendering_tier": self.rendering_tier,
        }
        for attr in ("width", "height", "framerate", "color_depth", "color_space"):
            v = getattr(self, attr)
            if v is not None:
                d[attr] = v
        if self.data_schema is not None:
            d["data_schema"] = self.data_schema.to_dict()
        return d


# ── RGB ───────────────────────────────────────────────────────────────────────

@dataclass
class RgbFormat:
    encoding: str = "rgb888"          # "rgb888","rgb565","rgbw","hsv","ddp","artnet","e131"
    led_count: int = 0
    framerate: float = 30.0
    zones: int | None = None
    color_depth: int = 8              # bits per channel (8 for RGB888)
    white_channel: bool = False
    gamma_corrected: bool = False

    @property
    def bandwidth_bps(self) -> int:
        channels = 4 if self.white_channel else 3
        return int(self.led_count * channels * self.color_depth * self.framerate)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "encoding": self.encoding,
            "led_count": self.led_count,
            "framerate": self.framerate,
            "color_depth": self.color_depth,
            "white_channel": self.white_channel,
            "gamma_corrected": self.gamma_corrected,
        }
        if self.zones is not None:
            d["zones"] = self.zones
        return d


# ── Control ───────────────────────────────────────────────────────────────────

@dataclass
class ControlInputSet:
    buttons: int = 0
    faders: int = 0
    encoders: int = 0
    xy_pads: int = 0
    axes: int = 0
    pressure_sensitive: bool = False
    touch_strips: int = 0

    def to_dict(self) -> dict:
        return {
            "buttons": self.buttons,
            "faders": self.faders,
            "encoders": self.encoders,
            "xy_pads": self.xy_pads,
            "axes": self.axes,
            "pressure_sensitive": self.pressure_sensitive,
            "touch_strips": self.touch_strips,
        }


@dataclass
class ControlOutputSet:
    button_leds: int = 0
    led_colors: int = 0
    motor_faders: int = 0
    led_rings: int = 0
    rumble_motors: int = 0

    def to_dict(self) -> dict:
        return {
            "button_leds": self.button_leds,
            "led_colors": self.led_colors,
            "motor_faders": self.motor_faders,
            "led_rings": self.led_rings,
            "rumble_motors": self.rumble_motors,
        }


@dataclass
class ControlFormat:
    protocol: str = "hid_gamepad"     # "midi","osc","hid_gamepad","hid_consumer","streamdeck","evdev","serial"
    inputs: ControlInputSet = field(default_factory=ControlInputSet)
    outputs: ControlOutputSet = field(default_factory=ControlOutputSet)
    report_rate_hz: int | None = None
    bidirectional: bool = False

    @property
    def bandwidth_bps(self) -> int:
        if self.report_rate_hz:
            # Rough estimate: buttons (1 byte each) + faders (2 bytes each)
            report_size = self.inputs.buttons + self.inputs.faders * 2 + self.inputs.encoders
            return self.report_rate_hz * max(report_size, 4) * 8
        return 0

    def to_dict(self) -> dict:
        d: dict[str, Any] = {
            "protocol": self.protocol,
            "inputs": self.inputs.to_dict(),
            "outputs": self.outputs.to_dict(),
            "bidirectional": self.bidirectional,
        }
        if self.report_rate_hz is not None:
            d["report_rate_hz"] = self.report_rate_hz
        return d


# ── Data ──────────────────────────────────────────────────────────────────────

@dataclass
class DataFormat:
    encoding: str = "json"            # "raw","protobuf","json","msgpack"
    schema: str | None = None
    max_message_size: int | None = None

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"encoding": self.encoding}
        if self.schema:
            d["schema"] = self.schema
        if self.max_message_size is not None:
            d["max_message_size"] = self.max_message_size
        return d


# ── Format (tagged union) ─────────────────────────────────────────────────────

@dataclass
class Format:
    """
    A concrete format for a single media stream.

    Exactly one of the media-type-specific fields should be set.
    """
    media_type: MediaType
    video: VideoFormat | None = None
    audio: AudioFormat | None = None
    hid: HidFormat | None = None
    screen: ScreenFormat | None = None
    rgb: RgbFormat | None = None
    control: ControlFormat | None = None
    data: DataFormat | None = None

    @property
    def bandwidth_bps(self) -> int:
        sub = self._sub()
        return sub.bandwidth_bps if sub and hasattr(sub, "bandwidth_bps") else 0

    def _sub(self):
        return {
            MediaType.video: self.video,
            MediaType.audio: self.audio,
            MediaType.hid: self.hid,
            MediaType.screen: self.screen,
            MediaType.rgb: self.rgb,
            MediaType.control: self.control,
            MediaType.data: self.data,
        }.get(self.media_type)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"media_type": self.media_type.value}
        sub = self._sub()
        if sub is not None:
            d[self.media_type.value] = sub.to_dict()
        return d


# ── FormatRange ───────────────────────────────────────────────────────────────

@dataclass
class NumericRange:
    """Closed interval [min, max] for a numeric capability."""
    min: float
    max: float

    def contains(self, value: float) -> bool:
        return self.min <= value <= self.max

    def intersect(self, other: "NumericRange") -> "NumericRange | None":
        lo = max(self.min, other.min)
        hi = min(self.max, other.max)
        return NumericRange(lo, hi) if lo <= hi else None

    def to_dict(self) -> dict:
        return {"min": self.min, "max": self.max}


@dataclass
class FormatRange:
    """
    Capability advertisement for one media type.

    Describes what a port *can* produce or consume, not what it is
    currently doing. Used for format negotiation.
    """
    media_type: MediaType

    # Video capability ranges
    video_codecs: list[str] = field(default_factory=list)
    video_resolution: tuple[NumericRange, NumericRange] | None = None  # (width_range, height_range)
    video_framerate: NumericRange | None = None
    video_bit_depths: list[int] = field(default_factory=list)
    video_lossy: bool | None = None   # None = both accepted

    # Audio capability ranges
    audio_codecs: list[str] = field(default_factory=list)
    audio_sample_rates: list[int] = field(default_factory=list)
    audio_channels: NumericRange | None = None
    audio_bit_depths: list[int] = field(default_factory=list)
    audio_lossy: bool | None = None

    # HID
    hid_device_types: list[str] = field(default_factory=list)
    hid_report_rate: NumericRange | None = None
    hid_protocols: list[str] = field(default_factory=list)

    # Screen
    screen_encodings: list[str] = field(default_factory=list)
    screen_resolution: tuple[NumericRange, NumericRange] | None = None
    screen_rendering_tiers: list[int] = field(default_factory=list)

    # RGB
    rgb_encodings: list[str] = field(default_factory=list)
    rgb_led_count: NumericRange | None = None

    # Control
    control_protocols: list[str] = field(default_factory=list)

    # Data
    data_encodings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict:
        d: dict[str, Any] = {"media_type": self.media_type.value}
        if self.video_codecs:
            d["video_codecs"] = list(self.video_codecs)
        if self.audio_codecs:
            d["audio_codecs"] = list(self.audio_codecs)
        if self.audio_sample_rates:
            d["audio_sample_rates"] = list(self.audio_sample_rates)
        if self.hid_device_types:
            d["hid_device_types"] = list(self.hid_device_types)
        if self.control_protocols:
            d["control_protocols"] = list(self.control_protocols)
        return d

    def intersect(self, other: "FormatRange") -> "FormatRange | None":
        """
        Compute the intersection of two FormatRanges.

        Returns None if the intersection is empty (no compatible formats).
        """
        if self.media_type != other.media_type:
            return None

        mt = self.media_type

        if mt == MediaType.video:
            codecs = list(set(self.video_codecs) & set(other.video_codecs))
            if not codecs:
                return None
            # Intersect resolution ranges
            w_range = h_range = None
            if self.video_resolution and other.video_resolution:
                w_i = self.video_resolution[0].intersect(other.video_resolution[0])
                h_i = self.video_resolution[1].intersect(other.video_resolution[1])
                if w_i is None or h_i is None:
                    return None
                w_range, h_range = w_i, h_i
            elif self.video_resolution:
                w_range, h_range = self.video_resolution
            elif other.video_resolution:
                w_range, h_range = other.video_resolution
            res = (w_range, h_range) if w_range else None

            fps = None
            if self.video_framerate and other.video_framerate:
                fps = self.video_framerate.intersect(other.video_framerate)
                if fps is None:
                    return None
            else:
                fps = self.video_framerate or other.video_framerate

            depths = list(set(self.video_bit_depths) & set(other.video_bit_depths)) \
                if self.video_bit_depths and other.video_bit_depths \
                else (self.video_bit_depths or other.video_bit_depths)

            return FormatRange(
                media_type=mt,
                video_codecs=codecs,
                video_resolution=res,
                video_framerate=fps,
                video_bit_depths=depths,
            )

        if mt == MediaType.audio:
            codecs = list(set(self.audio_codecs) & set(other.audio_codecs))
            if not codecs:
                return None
            rates = list(set(self.audio_sample_rates) & set(other.audio_sample_rates)) \
                if self.audio_sample_rates and other.audio_sample_rates \
                else (self.audio_sample_rates or other.audio_sample_rates)
            depths = list(set(self.audio_bit_depths) & set(other.audio_bit_depths)) \
                if self.audio_bit_depths and other.audio_bit_depths \
                else (self.audio_bit_depths or other.audio_bit_depths)
            return FormatRange(
                media_type=mt,
                audio_codecs=codecs,
                audio_sample_rates=rates,
                audio_bit_depths=depths,
            )

        if mt == MediaType.hid:
            dtypes = list(set(self.hid_device_types) & set(other.hid_device_types)) \
                if self.hid_device_types and other.hid_device_types \
                else (self.hid_device_types or other.hid_device_types)
            protocols = list(set(self.hid_protocols) & set(other.hid_protocols)) \
                if self.hid_protocols and other.hid_protocols \
                else (self.hid_protocols or other.hid_protocols)
            return FormatRange(
                media_type=mt,
                hid_device_types=dtypes,
                hid_protocols=protocols,
            )

        if mt == MediaType.screen:
            encs = list(set(self.screen_encodings) & set(other.screen_encodings)) \
                if self.screen_encodings and other.screen_encodings \
                else (self.screen_encodings or other.screen_encodings)
            return FormatRange(media_type=mt, screen_encodings=encs)

        if mt == MediaType.rgb:
            encs = list(set(self.rgb_encodings) & set(other.rgb_encodings)) \
                if self.rgb_encodings and other.rgb_encodings \
                else (self.rgb_encodings or other.rgb_encodings)
            return FormatRange(media_type=mt, rgb_encodings=encs)

        if mt == MediaType.control:
            protos = list(set(self.control_protocols) & set(other.control_protocols)) \
                if self.control_protocols and other.control_protocols \
                else (self.control_protocols or other.control_protocols)
            return FormatRange(media_type=mt, control_protocols=protos)

        if mt == MediaType.data:
            encs = list(set(self.data_encodings) & set(other.data_encodings)) \
                if self.data_encodings and other.data_encodings \
                else (self.data_encodings or other.data_encodings)
            return FormatRange(media_type=mt, data_encodings=encs)

        # Pass-through for mixed/power
        return FormatRange(media_type=mt)

    def can_produce(self, fmt: Format) -> bool:
        """Check if this FormatRange can produce the given concrete Format."""
        if fmt.media_type != self.media_type:
            return False
        mt = self.media_type
        if mt == MediaType.video and fmt.video:
            f = fmt.video
            if self.video_codecs and f.codec not in self.video_codecs:
                return False
            if self.video_framerate and not self.video_framerate.contains(f.framerate):
                return False
            return True
        if mt == MediaType.audio and fmt.audio:
            f = fmt.audio
            if self.audio_codecs and f.codec not in self.audio_codecs:
                return False
            if self.audio_sample_rates and f.sample_rate not in self.audio_sample_rates:
                return False
            return True
        if mt == MediaType.hid and fmt.hid:
            f = fmt.hid
            if self.hid_device_types and f.device_type not in self.hid_device_types:
                return False
            return True
        return True


# ── FormatSet ─────────────────────────────────────────────────────────────────

@dataclass
class FormatSet:
    """
    A port's capability advertisement: the set of formats it supports.

    One FormatRange per media type this port can carry.
    """
    formats: list[FormatRange] = field(default_factory=list)

    def for_media_type(self, mt: MediaType) -> FormatRange | None:
        for r in self.formats:
            if r.media_type == mt:
                return r
        return None

    def intersect(self, other: "FormatSet") -> "FormatSet":
        """
        Return the intersection: only formats supported by both sets.
        """
        result: list[FormatRange] = []
        for r in self.formats:
            other_r = other.for_media_type(r.media_type)
            if other_r is None:
                continue
            intersection = r.intersect(other_r)
            if intersection is not None:
                result.append(intersection)
        return FormatSet(formats=result)

    def is_empty(self) -> bool:
        return len(self.formats) == 0

    def to_dict(self) -> dict:
        return {"formats": [r.to_dict() for r in self.formats]}


# ── Format negotiation ────────────────────────────────────────────────────────

class NegotiationFailure(Exception):
    """Raised when format negotiation cannot find any compatible format."""
    pass


def enumerate_formats(port_sets: list[FormatSet]) -> FormatSet:
    """
    Phase 1 — Enumerate.

    Combine all FormatSets from a list of ports along a path.
    Returns the union of all advertised formats (i.e., the set of formats
    that at least one port supports — intersection is applied in Phase 2).
    """
    if not port_sets:
        return FormatSet()
    # Start with first, intersect progressively
    result = port_sets[0]
    for ps in port_sets[1:]:
        result = result.intersect(ps)
    return result


def restrict_formats(
    combined: FormatSet,
    required_formats: list[str] | None = None,
    forbidden_formats: list[str] | None = None,
) -> FormatSet:
    """
    Phase 2 — Restrict.

    Apply constraint filters to the combined FormatSet.
    Removes formats that violate required/forbidden constraints.
    """
    if not required_formats and not forbidden_formats:
        return combined

    result: list[FormatRange] = []
    for r in combined.formats:
        mt = r.media_type
        if mt == MediaType.video:
            codecs = [c for c in r.video_codecs
                      if (not forbidden_formats or c not in forbidden_formats)
                      and (not required_formats or c in required_formats or not r.video_codecs)]
            if not codecs and r.video_codecs:
                continue
            result.append(FormatRange(
                media_type=mt,
                video_codecs=codecs,
                video_resolution=r.video_resolution,
                video_framerate=r.video_framerate,
                video_bit_depths=r.video_bit_depths,
            ))
        elif mt == MediaType.audio:
            codecs = [c for c in r.audio_codecs
                      if (not forbidden_formats or c not in forbidden_formats)
                      and (not required_formats or c in required_formats or not r.audio_codecs)]
            if not codecs and r.audio_codecs:
                continue
            result.append(FormatRange(
                media_type=mt,
                audio_codecs=codecs,
                audio_sample_rates=r.audio_sample_rates,
                audio_bit_depths=r.audio_bit_depths,
            ))
        else:
            result.append(r)

    return FormatSet(formats=result)


def fixate_format(
    negotiated: FormatSet,
    media_type: MediaType,
    prefer_lossless: bool = False,
    prefer_hardware_codec: bool = False,
    target_resolution: tuple[int, int] | None = None,
    target_framerate: float | None = None,
    target_sample_rate: int | None = None,
    target_channels: int | None = None,
    target_bit_depth: int | None = None,
) -> Format:
    """
    Phase 3 — Fixate.

    Select one concrete Format from a negotiated FormatSet.

    Selection priority:
      1. MUST satisfy constraints (already applied in Phase 2)
      2. Minimize conversions (prefer native/lossless if prefer_lossless)
      3. Match preferences (resolution, framerate, codec, etc.)
      4. Prefer hardware codecs if prefer_hardware_codec
      5. Among equals, prefer lower bandwidth
    """
    r = negotiated.for_media_type(media_type)
    if r is None:
        raise NegotiationFailure(
            f"No negotiated format for media type {media_type.value}"
        )

    if media_type == MediaType.video:
        # Hardware codec preference order
        _hw_codecs = ["h265", "h264", "av1", "vp9", "mjpeg"]
        _lossless = ["raw"]
        codecs = r.video_codecs or ["raw"]

        if prefer_lossless:
            # prefer raw/lossless first
            ordered = sorted(
                codecs,
                key=lambda c: (0 if c in _lossless else 1,
                               _hw_codecs.index(c) if c in _hw_codecs else 99),
            )
        elif prefer_hardware_codec:
            ordered = sorted(
                codecs,
                key=lambda c: _hw_codecs.index(c) if c in _hw_codecs else 99,
            )
        else:
            ordered = codecs

        codec = ordered[0]
        width = target_resolution[0] if target_resolution else (
            int(r.video_resolution[0].max) if r.video_resolution else 1920
        )
        height = target_resolution[1] if target_resolution else (
            int(r.video_resolution[1].max) if r.video_resolution else 1080
        )
        fps = target_framerate if target_framerate else (
            r.video_framerate.max if r.video_framerate else 30.0
        )
        bit_depth = target_bit_depth or (
            max(r.video_bit_depths) if r.video_bit_depths else 8
        )
        return Format(
            media_type=media_type,
            video=VideoFormat(
                codec=codec,
                width=width,
                height=height,
                framerate=fps,
                bit_depth=bit_depth,
                lossy=(codec not in _lossless),
            ),
        )

    if media_type == MediaType.audio:
        codecs = r.audio_codecs or ["pcm"]
        _lossless_audio = ["pcm", "flac", "aes67"]

        if prefer_lossless:
            codec = next((c for c in _lossless_audio if c in codecs), codecs[0])
        else:
            codec = codecs[0]

        rates = sorted(r.audio_sample_rates or [48000], reverse=True)
        rate = target_sample_rate if target_sample_rate and target_sample_rate in rates \
            else rates[0]

        depths = sorted(r.audio_bit_depths or [16], reverse=True)
        depth = target_bit_depth if target_bit_depth and target_bit_depth in depths \
            else depths[0]

        ch_range = r.audio_channels
        channels = target_channels if target_channels else (
            int(ch_range.max) if ch_range else 2
        )

        return Format(
            media_type=media_type,
            audio=AudioFormat(
                codec=codec,
                sample_rate=rate,
                channels=channels,
                bit_depth=depth,
                lossy=(codec not in _lossless_audio),
            ),
        )

    if media_type == MediaType.hid:
        dtypes = r.hid_device_types or ["keyboard"]
        protocols = r.hid_protocols or ["report"]
        return Format(
            media_type=media_type,
            hid=HidFormat(
                device_type=dtypes[0],
                protocol=protocols[0],
            ),
        )

    if media_type == MediaType.screen:
        encs = r.screen_encodings or ["jpeg"]
        tiers = r.screen_rendering_tiers or [0]
        return Format(
            media_type=media_type,
            screen=ScreenFormat(
                encoding=encs[0],
                rendering_tier=max(tiers),
            ),
        )

    if media_type == MediaType.rgb:
        encs = r.rgb_encodings or ["rgb888"]
        return Format(
            media_type=media_type,
            rgb=RgbFormat(encoding=encs[0]),
        )

    if media_type == MediaType.control:
        protos = r.control_protocols or ["hid_gamepad"]
        return Format(
            media_type=media_type,
            control=ControlFormat(protocol=protos[0]),
        )

    if media_type == MediaType.data:
        encs = r.data_encodings or ["json"]
        return Format(
            media_type=media_type,
            data=DataFormat(encoding=encs[0]),
        )

    raise NegotiationFailure(f"Unsupported media type: {media_type.value}")


def negotiate_format(
    port_sets: list[FormatSet],
    media_type: MediaType,
    required_formats: list[str] | None = None,
    forbidden_formats: list[str] | None = None,
    prefer_lossless: bool = False,
    prefer_hardware_codec: bool = False,
    target_resolution: tuple[int, int] | None = None,
    target_framerate: float | None = None,
    target_sample_rate: int | None = None,
    target_channels: int | None = None,
    target_bit_depth: int | None = None,
) -> Format:
    """
    Run all three phases and return a concrete Format.

    Raises NegotiationFailure if no compatible format can be found.
    """
    combined = enumerate_formats(port_sets)
    restricted = restrict_formats(combined, required_formats, forbidden_formats)
    return fixate_format(
        restricted,
        media_type,
        prefer_lossless=prefer_lossless,
        prefer_hardware_codec=prefer_hardware_codec,
        target_resolution=target_resolution,
        target_framerate=target_framerate,
        target_sample_rate=target_sample_rate,
        target_channels=target_channels,
        target_bit_depth=target_bit_depth,
    )


# ── Bandwidth helpers ─────────────────────────────────────────────────────────

def video_bandwidth_uncompressed(
    width: int,
    height: int,
    bit_depth: int = 8,
    framerate: float = 30.0,
    channels: int = 3,
) -> int:
    """Uncompressed video bandwidth in bits per second."""
    return int(width * height * bit_depth * channels * framerate)


def audio_bandwidth_uncompressed(
    sample_rate: int,
    channels: int,
    bit_depth: int,
) -> int:
    """Uncompressed audio bandwidth in bits per second."""
    return sample_rate * channels * bit_depth


def hid_bandwidth(report_rate_hz: int, report_size_bytes: int) -> int:
    """HID bandwidth in bits per second."""
    return report_rate_hz * report_size_bytes * 8


def screen_bandwidth_raw(
    width: int,
    height: int,
    color_depth: int,
    framerate: float,
) -> int:
    """Raw screen frame bandwidth in bits per second."""
    return int(width * height * color_depth * framerate)


def rgb_bandwidth(
    led_count: int,
    color_depth: int = 8,
    framerate: float = 30.0,
    white_channel: bool = False,
) -> int:
    """RGB LED strip bandwidth in bits per second."""
    channels = 4 if white_channel else 3
    return int(led_count * channels * color_depth * framerate)


def serial_bandwidth_bytes_per_sec(
    baud_rate: int,
    data_bits: int = 8,
    parity: str = "none",
    stop_bits: float = 1.0,
) -> int:
    """Effective serial throughput in bytes per second."""
    parity_bits = 0 if parity == "none" else 1
    total_bits = 1 + data_bits + parity_bits + stop_bits  # start + data + parity + stop
    return int(baud_rate / total_bits)
