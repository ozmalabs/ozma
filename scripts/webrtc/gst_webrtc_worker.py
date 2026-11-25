#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
GStreamer WebRTC worker — runs under system Python with GStreamer bindings.

Reads raw BGRA framebuffer from shared memory, encodes with NVENC (or x264),
serves via webrtcbin. Signaling via Unix socket.

The pipeline can optionally carry system audio from a PulseAudio/PipeWire
monitor source.  Audio is opt-in: without ``--audio-source`` the pipeline is
exactly the video-only pipeline used before audio support existed.

GStreamer is imported lazily so the pure pipeline/argument helpers in this
module can be imported and tested on hosts without ``gi`` installed.
"""

import argparse
import asyncio
import json
import os
import sys
import time
import threading
from urllib.parse import quote

# Populated by `load_gstreamer()`; `None` until then.
Gst = None
GstWebRTC = None
GstSdp = None
GLib = None

#: Name given to the video encoder element in the pipeline string.
VIDEO_ENCODER_NAME = "venc"
#: Name given to the audio source element in the pipeline string.
AUDIO_SOURCE_NAME = "asrc"

#: Bitrate bounds accepted over signaling, in bits per second.
MIN_BITRATE_BPS = 500_000
MAX_BITRATE_BPS = 50_000_000

#: Bounded wait for one SDP description operation, in seconds.
DESCRIPTION_TIMEOUT_SECONDS = 5.0
#: Bounded wait for ICE gathering after the local answer is set, in seconds.
ICE_GATHER_TIMEOUT_SECONDS = 5.0

#: Schemes accepted by the STUN/TURN URI normalisers.
STUN_SCHEMES = frozenset({"stun", "stuns"})
TURN_SCHEMES = frozenset({"turn", "turns"})

#: ICE candidate types emitted by webrtcbin that this worker can name.
CANDIDATE_TYPES = frozenset({"host", "srflx", "prflx", "relay"})

#: Elements required by the optional audio branch (besides the source).
AUDIO_PIPELINE_ELEMENTS = frozenset(
    {"queue", "audioconvert", "audioresample", "opusenc", "rtpopuspay"}
)

#: Source element (and the property carrying the monitor name) per backend.
_AUDIO_SOURCE_ELEMENTS = {
    "pulse": ("pulsesrc", "device"),
    "pipewire": ("pipewiresrc", "target-object"),
}


def load_gstreamer() -> bool:
    """Import and initialise GStreamer, returning False when unavailable.

    Keeping this out of module import time lets ``python3 -m py_compile`` and
    the pure helper tests run on hosts without GStreamer.
    """
    global Gst, GstWebRTC, GstSdp, GLib
    if Gst is not None:
        return True

    try:
        import gi

        gi.require_version("Gst", "1.0")
        gi.require_version("GstWebRTC", "1.0")
        gi.require_version("GstSdp", "1.0")
        from gi.repository import GLib as _GLib
        from gi.repository import Gst as _Gst
        from gi.repository import GstSdp as _GstSdp
        from gi.repository import GstWebRTC as _GstWebRTC
    except (ImportError, ValueError) as error:
        print(
            f"GStreamer Python bindings are unavailable: {error}",
            file=sys.stderr,
            flush=True,
        )
        return False

    _Gst.init(None)
    Gst = _Gst
    GstWebRTC = _GstWebRTC
    GstSdp = _GstSdp
    GLib = _GLib
    return True


def video_encoder_description(use_nvenc: bool) -> str:
    """Return the video encoder element description (named ``venc``)."""
    if use_nvenc:
        return (
            f"nvh264enc name={VIDEO_ENCODER_NAME} preset=low-latency-hq "
            "rc-mode=cbr bitrate=4000 zerolatency=true"
        )
    return (
        f"x264enc name={VIDEO_ENCODER_NAME} tune=zerolatency "
        "speed-preset=ultrafast bitrate=3000 key-int-max=30"
    )


def select_audio_source_element(
    backend: str, available: set[str] | frozenset[str]
) -> tuple[str, str] | None:
    """Choose ``(element, property)`` for the requested audio backend.

    ``auto`` prefers ``pulsesrc`` because it also works against PipeWire's
    PulseAudio compatibility server, then falls back to ``pipewiresrc``.
    An explicit backend never falls back to the other one.
    """
    if backend == "auto":
        candidates = ("pulsesrc", "pipewiresrc")
    elif backend in _AUDIO_SOURCE_ELEMENTS:
        candidates = (_AUDIO_SOURCE_ELEMENTS[backend][0],)
    else:
        return None
    for element in candidates:
        if element in available:
            property_name = next(
                prop
                for candidate, prop in _AUDIO_SOURCE_ELEMENTS.values()
                if candidate == element
            )
            return element, property_name
    return None


def _split_ice_address(value: str, schemes: frozenset[str], default: str):
    """Return ``(scheme, host[:port])`` for a STUN/TURN address.

    Accepts webrtcbin URI form (``stun://host:port``), the common
    ``stun:host:port`` form, and a bare ``host:port``. Credentials are never
    accepted in this part of the address.
    """
    value = (value or "").strip()
    if not value:
        raise ValueError("ICE server address must not be empty")
    if any(character.isspace() for character in value):
        raise ValueError("ICE server address must not contain whitespace")
    if "@" in value:
        raise ValueError(
            "ICE server address must not embed credentials; use the TURN "
            "username/password arguments"
        )
    scheme, separator, rest = value.partition("://")
    if not separator:
        head, colon, tail = value.partition(":")
        if colon and head.lower() in schemes:
            scheme, rest = head, tail
        elif colon and head.lower() in (STUN_SCHEMES | TURN_SCHEMES):
            raise ValueError(
                f"unsupported ICE scheme {head.lower()!r}; expected one of "
                f"{', '.join(sorted(schemes))}"
            )
        else:
            scheme, rest = default, value
    scheme = scheme.lower()
    if scheme not in schemes:
        raise ValueError(
            f"unsupported ICE scheme {scheme!r}; expected one of "
            f"{', '.join(sorted(schemes))}"
        )
    rest = rest.strip()
    if not rest:
        raise ValueError("ICE server address is missing a host")
    return scheme, rest


def normalize_stun_server(value: str) -> str:
    """Return webrtcbin's ``stun://host:port`` form for *value*."""
    scheme, rest = _split_ice_address(value, STUN_SCHEMES, "stun")
    return f"{scheme}://{rest}"


def normalize_turn_address(value: str) -> tuple[str, str]:
    """Return ``(scheme, host[:port])`` for a TURN address without credentials."""
    return _split_ice_address(value, TURN_SCHEMES, "turn")


def build_turn_uri(
    server: str, username: str | None, password: str | None
) -> str:
    """Build webrtcbin's ``turn://user:password@host:port`` property value.

    The username and password are percent-encoded so a colon, at-sign, slash,
    or base64 padding cannot change the URI's authority. The password is never
    returned in logs or status by the worker.
    """
    if not username or not password:
        raise ValueError("TURN requires both a username and a password")
    scheme, rest = normalize_turn_address(server)
    user = quote(username, safe="")
    secret = quote(password, safe="")
    return f"{scheme}://{user}:{secret}@{rest}"


def candidate_type(candidate: str) -> str | None:
    """Return the candidate's ICE type (``host``/``srflx``/``prflx``/``relay``).

    ``None`` means the candidate did not carry a recognised ``typ`` token. The
    worker only labels candidates it actually received from webrtcbin; it never
    invents a candidate.
    """
    value = candidate.strip()
    if value.startswith("a="):
        value = value[2:]
    parts = value.split()
    try:
        index = parts.index("typ")
    except ValueError:
        return None
    if index + 1 >= len(parts):
        return None
    kind = parts[index + 1].lower()
    return kind if kind in CANDIDATE_TYPES else None


def candidate_type_counts(candidates) -> dict[str, int]:
    """Count the ICE types in ``(mline, candidate)`` pairs."""
    counts: dict[str, int] = {}
    for _mline, candidate in candidates:
        kind = candidate_type(candidate)
        if kind is not None:
            counts[kind] = counts.get(kind, 0) + 1
    return counts


def validate_stun_turn_args(args) -> None:
    """Validate CLI ICE arguments, raising ``ValueError`` for argparse."""
    if args.stun_server:
        normalize_stun_server(args.stun_server)
    if args.turn_server:
        build_turn_uri(args.turn_server, args.turn_user, args.turn_password)
    elif args.turn_user or args.turn_password:
        raise ValueError("--turn-user/--turn-password require --turn-server")


def build_pipeline_string(
    width: int,
    height: int,
    fps: int,
    encoder: str,
    audio_element: str | None = None,
    video_pt: int = 96,
    audio_pt: int = 97,
) -> str:
    """Build the ``gst_parse_launch`` string.

    With ``audio_element`` omitted the result is the original video-only
    pipeline.  With it set, video and audio are linked as separate request
    pads on one bundled ``webrtcbin``; the monitor name itself is applied with
    ``set_property`` afterwards so it never has to be escaped into the string.

    ``video_pt``/``audio_pt`` must be the payload types from the peer's offer.
    A browser does not use the GStreamer client's 96/97 (Chrome, for example,
    maps 96 to VP8), so a hard-coded payloader PT makes the answer unusable.
    """
    if audio_element not in (None, "pulsesrc", "pipewiresrc"):
        raise ValueError(f"unsupported audio source element: {audio_element}")
    if not 0 < video_pt < 128:
        raise ValueError(f"invalid video payload type: {video_pt}")
    if not 0 < audio_pt < 128:
        raise ValueError(f"invalid audio payload type: {audio_pt}")

    video_chain = (
        f"appsrc name=src format=time is-live=true do-timestamp=true "
        f"caps=video/x-raw,format=BGRA,width={width},height={height},"
        f"framerate={fps}/1 "
        f"! videoconvert "
        f"! {encoder} "
        f"! video/x-h264,profile=baseline "
        f"! rtph264pay config-interval=-1 pt={video_pt} "
    )

    if audio_element is None:
        return (
            video_chain
            + "! webrtcbin name=webrtc bundle-policy=max-bundle"
        )

    audio_chain = (
        f"{audio_element} name={AUDIO_SOURCE_NAME} "
        f"! queue "
        f"! audioconvert "
        f"! audioresample "
        f"! audio/x-raw,format=S16LE,rate=48000,channels=2 "
        f"! opusenc name=aenc bitrate=96000 "
        f"! rtpopuspay pt={audio_pt} "
        f"! application/x-rtp,media=audio,encoding-name=OPUS,"
        f"payload={audio_pt},clock-rate=48000 "
        f"! webrtc."
    )
    return (
        f"webrtcbin name=webrtc bundle-policy=max-bundle "
        + video_chain
        + f"! application/x-rtp,media=video,encoding-name=H264,payload={video_pt} "
        + "! webrtc. "
        + audio_chain
    )


def clamp_bitrate_bps(requested: int) -> int:
    """Clamp a requested bitrate to the supported 500 kbps–50 Mbps range."""
    return max(MIN_BITRATE_BPS, min(MAX_BITRATE_BPS, int(requested)))


def split_sdp_media_sections(sdp: str) -> tuple[list[str], list[list[str]]]:
    """Split an SDP into preamble and per-``m=`` sections.

    Each section starts with its ``m=`` line and keeps every following line.
    Line endings are normalised away so SDPs with CRLF and LF behave the same.
    """
    preamble: list[str] = []
    sections: list[list[str]] = []
    current: list[str] | None = None
    for raw_line in sdp.splitlines():
        line = raw_line.rstrip("\r")
        if line.startswith("m="):
            current = [line]
            sections.append(current)
        elif current is None:
            preamble.append(line)
        else:
            current.append(line)
    return preamble, sections


def remote_ice_candidates(sdp: str) -> list[tuple[int, str]]:
    """Return ``(mline_index, candidate)`` for every ``a=candidate:`` line.

    The candidate string keeps its ``candidate:`` prefix, matching the value
    emitted by webrtcbin's ``on-ice-candidate`` signal, so it can be passed
    straight back to ``add-ice-candidate``.
    """
    _, sections = split_sdp_media_sections(sdp)
    candidates: list[tuple[int, str]] = []
    for index, lines in enumerate(sections):
        for line in lines:
            if not line.startswith("a=candidate:"):
                continue
            candidate = line[len("a=") :].strip()
            if candidate and not candidate.startswith("candidate:end-of-candidates"):
                candidates.append((index, candidate))
    return candidates


def offered_payload_type(sdp: str, media: str, encoding: str) -> int | None:
    """Return the peer's payload type for ``encoding`` on the ``media`` m-line.

    A browser chooses dynamic payload types (Chrome often maps 96 to VP8 and
    H.264 to 103+), so the answer must reuse the offered value rather than a
    fixed 96/97.  Only the first matching m-line is considered; ``None`` means
    the peer did not offer that codec.
    """
    _, sections = split_sdp_media_sections(sdp)
    for section in sections:
        header = section[0].split()
        if len(header) < 4 or header[0] != f"m={media}":
            continue
        offered = set(header[3:])
        for line in section[1:]:
            if not line.startswith("a=rtpmap:"):
                continue
            try:
                pt_text, encoding_text = line[len("a=rtpmap:"):].split(None, 1)
                payload_type = int(pt_text)
            except (ValueError, IndexError):
                continue
            if (
                str(payload_type) in offered
                and encoding_text.split("/", 1)[0].lower() == encoding.lower()
            ):
                return payload_type
        return None
    return None


def append_ice_candidates(
    sdp: str, candidates: list[tuple[int, str]]
) -> str:
    """Append ``a=candidate`` lines to their matching ``m=`` sections.

    ``candidates`` entries use the same ``(mline_index, candidate)`` shape as
    :func:`remote_ice_candidates`; the candidate may include or omit the
    ``candidate:`` prefix.  Sections without a matching index are ignored, and
    an SDP with no ``m=`` line is returned unchanged.
    """
    if not candidates:
        return sdp
    preamble, sections = split_sdp_media_sections(sdp)
    if not sections:
        return sdp

    by_index: dict[int, list[str]] = {}
    for index, candidate in candidates:
        value = candidate.strip()
        if value.startswith("a=candidate:"):
            value = value[len("a=") :]
        elif not value.startswith("candidate:"):
            value = "candidate:" + value
        by_index.setdefault(index, []).append(value)

    lines = list(preamble)
    for index, section in enumerate(sections):
        existing = {line for line in section if line.startswith("a=candidate:")}
        additions = [
            "a=" + candidate
            for candidate in by_index.get(index, [])
            if "a=" + candidate not in existing
        ]
        if additions:
            # Keep `a=end-of-candidates` as the last attribute in the section.
            position = next(
                (
                    offset
                    for offset, line in enumerate(section)
                    if line.startswith("a=end-of-candidates")
                ),
                len(section),
            )
            lines.extend(section[:position])
            lines.extend(additions)
            lines.extend(section[position:])
        else:
            lines.extend(section)
    return "\r\n".join(lines) + "\r\n"


def build_arg_parser() -> argparse.ArgumentParser:
    """Build the worker CLI parser (pure, importable without GStreamer)."""
    parser = argparse.ArgumentParser(
        description="GStreamer WebRTC worker with optional monitor audio",
    )
    parser.add_argument("--sock", required=True, help="signaling Unix socket")
    parser.add_argument("--shm", required=True, help="shared-memory frame path")
    parser.add_argument("--width", type=int, default=640)
    parser.add_argument("--height", type=int, default=480)
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument(
        "--audio-source",
        default=None,
        help=(
            "PulseAudio/PipeWire monitor source to capture, for example "
            "`ozma-vm1.monitor`; omit for video-only WebRTC"
        ),
    )
    parser.add_argument(
        "--audio-backend",
        choices=("auto", "pulse", "pipewire"),
        default="auto",
        help=(
            "audio capture backend (default: auto, which prefers pulsesrc and "
            "falls back to pipewiresrc); ignored without --audio-source"
        ),
    )
    parser.add_argument(
        "--stun-server",
        default=None,
        help=(
            "STUN server (stun://host:port, stun:host:port, or host:port); "
            "omit to keep host-candidate-only gathering"
        ),
    )
    parser.add_argument(
        "--turn-server",
        default=None,
        help=(
            "TURN server address without credentials (turn://host:port, "
            "turn:host:port, or host:port); requires --turn-user and "
            "--turn-password"
        ),
    )
    parser.add_argument(
        "--turn-user",
        default=None,
        help="TURN username; requires --turn-server",
    )
    parser.add_argument(
        "--turn-password",
        default=None,
        help="TURN password; requires --turn-server and is never logged",
    )
    return parser


class WebRTCWorker:
    def __init__(
        self,
        shm_path: str,
        width: int,
        height: int,
        fps: int,
        audio_source: str | None = None,
        audio_backend: str = "auto",
        stun_server: str | None = None,
        turn_server: str | None = None,
        turn_user: str | None = None,
        turn_password: str | None = None,
    ):
        self.shm_path = shm_path
        self.width = width
        self.height = height
        self.fps = fps
        self.audio_source = audio_source or None
        self.audio_backend = audio_backend
        self.stun_server = (
            normalize_stun_server(stun_server) if stun_server else None
        )
        self.turn_uri = (
            build_turn_uri(turn_server, turn_user, turn_password)
            if turn_server
            else None
        )
        self.sessions = {}
        self.mainloop = None

    def ice_properties(self) -> dict[str, str]:
        """Return the webrtcbin ICE properties for this worker.

        Empty when no STUN/TURN server was configured, which preserves the
        historical host-candidate-only pipeline. The TURN value contains
        credentials and must never be logged.
        """
        properties = {}
        if self.stun_server:
            properties["stun-server"] = self.stun_server
        if self.turn_uri:
            properties["turn-server"] = self.turn_uri
        return properties

    # -- optional audio ---------------------------------------------------

    def _select_audio(self) -> tuple[str, str] | None:
        """Resolve the audio source element, or None with a clear log."""
        if not self.audio_source:
            return None

        available = {
            name
            for name in (
                "pulsesrc",
                "pipewiresrc",
                *AUDIO_PIPELINE_ELEMENTS,
            )
            if Gst.ElementFactory.find(name)
        }
        selected = select_audio_source_element(self.audio_backend, available)
        if selected is None:
            print(
                f"WebRTC audio requested from {self.audio_source!r} but no "
                f"capture element is available for backend "
                f"{self.audio_backend!r}; continuing video-only",
                file=sys.stderr,
                flush=True,
            )
            return None

        missing = sorted(AUDIO_PIPELINE_ELEMENTS - available)
        if missing:
            print(
                "WebRTC audio requested but these GStreamer elements are "
                f"missing: {', '.join(missing)}; continuing video-only",
                file=sys.stderr,
                flush=True,
            )
            return None
        return selected

    def _build_pipeline(
        self, use_nvenc: bool, video_pt: int = 96, audio_pt: int = 97
    ):
        """Build the session pipeline, degrading to video-only on audio errors."""
        encoder = video_encoder_description(use_nvenc)
        audio = self._select_audio()
        audio_element = audio[0] if audio else None
        pipeline = None
        try:
            pipeline = Gst.parse_launch(
                build_pipeline_string(
                    self.width,
                    self.height,
                    self.fps,
                    encoder,
                    audio_element,
                    video_pt,
                    audio_pt,
                )
            )
            if audio is not None:
                _, property_name = audio
                asrc = pipeline.get_by_name(AUDIO_SOURCE_NAME)
                if asrc is None:
                    raise RuntimeError("audio source element was not created")
                asrc.set_property(property_name, self.audio_source)
        except Exception as error:
            if pipeline is not None:
                pipeline.set_state(Gst.State.NULL)
            if audio_element is None and pipeline is None:
                raise
            print(
                f"WebRTC audio branch could not be built ({error}); "
                "continuing with video-only",
                file=sys.stderr,
                flush=True,
            )
            pipeline = Gst.parse_launch(
                build_pipeline_string(
                    self.width, self.height, self.fps, encoder, None, video_pt
                )
            )
            audio = None
        return pipeline, audio is not None

    # -- sessions ---------------------------------------------------------

    def create_session(self, offer_sdp: str) -> tuple[str, str] | None:
        """Create a WebRTC session from an SDP offer.

        Returns ``(answer_sdp, session_id)``. The answer carries every local
        candidate gathered before the bounded gathering wait completes, so a
        peer that never sends a trickle message still works. The session id lets
        a peer deliver late remote candidates through
        :meth:`add_ice_candidate`.
        """
        import uuid
        sid = uuid.uuid4().hex[:8]

        # Reuse the payload types the peer actually offered.  Browsers do not
        # use the GStreamer client's 96/97 (Chrome maps 96 to VP8), and a
        # mismatched payloader makes webrtcbin answer the m-line `inactive`.
        video_pt = offered_payload_type(offer_sdp, "video", "H264") or 96
        audio_pt = offered_payload_type(offer_sdp, "audio", "OPUS") or 97

        nvenc = Gst.ElementFactory.find("nvh264enc")
        pipeline, audio_enabled = self._build_pipeline(
            bool(nvenc), video_pt, audio_pt
        )
        webrtc = pipeline.get_by_name("webrtc")
        appsrc = pipeline.get_by_name("src")
        video_encoder = pipeline.get_by_name(VIDEO_ENCODER_NAME)

        # STUN/TURN are only applied when configured. Without them the element
        # keeps its historical host-candidate-only behavior.
        ice_properties = self.ice_properties()
        for property_name, value in ice_properties.items():
            webrtc.set_property(property_name, value)

        session = {
            "pipeline": pipeline,
            "appsrc": appsrc,
            "video_encoder": video_encoder,
            "webrtc": webrtc,
            "audio": audio_enabled,
            "local_candidates": [],
            "remote_candidates": [],
            "gather_complete": False,
        }
        self.sessions[sid] = session

        # `create-answer` returns no answer when the bin is still in NULL.
        pipeline.set_state(Gst.State.PLAYING)
        result, _, _ = pipeline.get_state(Gst.CLOCK_TIME_NONE)
        if result == Gst.StateChangeReturn.FAILURE:
            pipeline.set_state(Gst.State.NULL)
            self.sessions.pop(sid, None)
            print("WebRTC pipeline failed to reach PLAYING", file=sys.stderr,
                  flush=True)
            return None

        # Apply the remote offer, then hand its embedded candidates to ICE.
        _, sdpmsg = GstSdp.SDPMessage.new_from_text(offer_sdp)
        offer = GstWebRTC.WebRTCSessionDescription.new(
            GstWebRTC.WebRTCSDPType.OFFER, sdpmsg)
        remote_done = threading.Event()
        remote_error = []

        def on_remote(promise):
            reply = promise.get_reply()
            error = reply.get_value("error") if reply else None
            if error is not None:
                remote_error.append(str(error))
            remote_done.set()

        webrtc.emit(
            "set-remote-description",
            offer,
            Gst.Promise.new_with_change_func(on_remote),
        )
        if not remote_done.wait(timeout=DESCRIPTION_TIMEOUT_SECONDS):
            remote_error.append("timed out setting the remote description")
        if remote_error:
            print(
                f"WebRTC remote description failed: {remote_error[0]}",
                file=sys.stderr,
                flush=True,
            )
            pipeline.set_state(Gst.State.NULL)
            self.sessions.pop(sid, None)
            return None

        remote_candidates = remote_ice_candidates(offer_sdp)
        session["remote_candidates"].extend(remote_candidates)
        for mline, candidate in remote_candidates:
            webrtc.emit("add-ice-candidate", mline, candidate)

        # Collect local candidates; the ones already known travel inside the
        # answer so a peer that does not poll for late candidates still works.
        local_candidates: list[tuple[int, str]] = session["local_candidates"]
        gather_complete = threading.Event()

        def on_candidate(_webrtc, mline, candidate):
            if candidate:
                local_candidates.append((int(mline), candidate))

        def on_gathering_state(*_args):
            state = webrtc.get_property("ice-gathering-state")
            if state == GstWebRTC.WebRTCICEGatheringState.COMPLETE:
                session["gather_complete"] = True
                gather_complete.set()

        webrtc.connect("on-ice-candidate", on_candidate)
        webrtc.connect("notify::ice-gathering-state", on_gathering_state)

        # Create the answer and install it as the local description.
        answer_sdp = [None]
        answer_done = threading.Event()
        answer_error = []

        def on_answer(promise):
            reply = promise.get_reply()
            error = reply.get_value("error") if reply else None
            if error is not None:
                answer_error.append(str(error))
            answer = reply.get_value("answer") if reply else None
            if answer:
                webrtc.emit("set-local-description", answer, Gst.Promise.new())
                answer_sdp[0] = answer.sdp.as_text()
            answer_done.set()

        webrtc.emit(
            "create-answer",
            None,
            Gst.Promise.new_with_change_func(on_answer),
        )
        if not answer_done.wait(timeout=DESCRIPTION_TIMEOUT_SECONDS):
            answer_error.append("timed out creating the answer")
        if not answer_sdp[0] or answer_error:
            if answer_error:
                print(
                    f"WebRTC create-answer failed: {answer_error[0]}",
                    file=sys.stderr,
                    flush=True,
                )
            pipeline.set_state(Gst.State.NULL)
            self.sessions.pop(sid, None)
            return None

        if not gather_complete.wait(timeout=ICE_GATHER_TIMEOUT_SECONDS):
            print(
                "WebRTC ICE gathering did not complete before the answer was "
                f"sent; returning {len(local_candidates)} candidate(s) anyway",
                file=sys.stderr,
                flush=True,
            )
        answer_sdp[0] = append_ice_candidates(answer_sdp[0], local_candidates)

        # Start frame feeder
        t = threading.Thread(target=self._feed, args=(sid, appsrc), daemon=True)
        t.start()

        enc_name = "NVENC" if nvenc else "x264"
        audio_name = f" audio={self.audio_source}" if audio_enabled else ""
        types = candidate_type_counts(local_candidates)
        type_summary = ",".join(
            f"{kind}={count}" for kind, count in sorted(types.items())
        ) or "none"
        ice_summary = "+".join(sorted(ice_properties)) or "none"
        print(
            f"Session {sid}: {self.width}x{self.height}@{self.fps} "
            f"{enc_name}{audio_name} ice={ice_summary} "
            f"ice_candidates={len(local_candidates)} ({type_summary}) "
            f"remote_candidates={len(session['remote_candidates'])}",
            flush=True,
        )
        return answer_sdp[0], sid

    def add_ice_candidate(
        self, session_id: str | None, candidate: str, sdp_mline_index: int = 0
    ) -> dict:
        """Add one trickled remote ICE candidate to an existing session.

        When ``session_id`` is omitted the candidate is applied to every live
        session, which keeps old single-session clients working. The candidate
        is validated before it reaches webrtcbin and is never echoed back.
        """
        value = (candidate or "").strip()
        if not value:
            return {"ok": False, "error": "candidate must not be empty"}
        if any(
            character != " "
            and (ord(character) < 0x20 or ord(character) == 0x7F)
            for character in value
        ):
            return {"ok": False, "error": "candidate contains control characters"}
        if value.startswith("a=candidate:"):
            value = value[len("a="):]
        if not value.startswith("candidate:"):
            return {"ok": False, "error": "candidate must start with `candidate:`"}
        if candidate_type(value) is None:
            return {
                "ok": False,
                "error": "candidate must carry a recognised `typ` token",
            }
        try:
            mline = int(sdp_mline_index)
        except (TypeError, ValueError):
            return {"ok": False, "error": "sdpMLineIndex must be an integer"}
        if mline < 0:
            return {"ok": False, "error": "sdpMLineIndex must not be negative"}

        if session_id:
            sessions = [self.sessions[session_id]] if session_id in self.sessions else []
            if not sessions:
                return {"ok": False, "error": "unknown WebRTC session"}
        else:
            sessions = list(self.sessions.values())
            if not sessions:
                return {"ok": False, "error": "no live WebRTC session"}

        for session in sessions:
            session["webrtc"].emit("add-ice-candidate", mline, value)
            session["remote_candidates"].append((mline, value))
        return {
            "ok": True,
            "session": session_id,
            "remote_candidates": sum(
                len(session["remote_candidates"]) for session in sessions
            ),
        }

    def set_bitrate(self, requested) -> dict:
        """Apply a video bitrate (bps) to every live session's encoder.

        Returns the signaling reply.  Success requires at least one encoder
        element that accepted the property change; without one this is an
        explicit ``{"ok": false, ...}`` rather than a fabricated ack.
        """
        try:
            requested = int(requested)
        except (TypeError, ValueError):
            return {"ok": False, "error": "bitrate must be an integer in bps"}

        clamped = clamp_bitrate_bps(requested)
        # GStreamer's nvh264enc/x264enc `bitrate` property is kbps.
        kbps = max(1, clamped // 1000)

        applied = 0
        errors = []
        for session in list(self.sessions.values()):
            encoder = session.get("video_encoder")
            if encoder is None:
                continue
            try:
                encoder.set_property("bitrate", kbps)
                applied += 1
            except Exception as error:
                errors.append(str(error))

        if applied == 0:
            error = errors[0] if errors else "no video encoder is available"
            return {"ok": False, "error": error}
        return {"ok": True, "bitrate": clamped}

    def _feed(self, sid, appsrc):
        """Push frames from shared memory to appsrc."""
        interval = 1.0 / self.fps
        pts = 0
        duration = Gst.SECOND // self.fps
        frame_size = self.width * self.height * 4

        while sid in self.sessions:
            try:
                if os.path.exists(self.shm_path):
                    with open(self.shm_path, "rb") as f:
                        data = f.read(frame_size)
                    if len(data) == frame_size:
                        buf = Gst.Buffer.new_wrapped(data)
                        buf.pts = pts
                        buf.duration = duration
                        pts += duration
                        ret = appsrc.emit("push-buffer", buf)
                        if ret != Gst.FlowReturn.OK:
                            break
            except Exception:
                pass
            time.sleep(interval)

        session = self.sessions.pop(sid, None)
        if session:
            session["pipeline"].set_state(Gst.State.NULL)


async def main():
    parser = build_arg_parser()
    args = parser.parse_args()
    try:
        validate_stun_turn_args(args)
    except ValueError as error:
        parser.error(str(error))

    if not load_gstreamer():
        return 1

    worker = WebRTCWorker(
        args.shm,
        args.width,
        args.height,
        args.fps,
        audio_source=args.audio_source,
        audio_backend=args.audio_backend,
        stun_server=args.stun_server,
        turn_server=args.turn_server,
        turn_user=args.turn_user,
        turn_password=args.turn_password,
    )
    worker.mainloop = GLib.MainLoop()

    # Start GLib main loop in a thread
    glib_thread = threading.Thread(target=worker.mainloop.run, daemon=True)
    glib_thread.start()

    # Signaling server on Unix socket
    try:
        os.unlink(args.sock)
    except FileNotFoundError:
        pass

    async def handle_client(reader, writer):
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                msg = json.loads(line)
                if msg.get("type") == "offer":
                    result = worker.create_session(msg["sdp"])
                    if result:
                        answer, session = result
                        resp = json.dumps(
                            {"type": "answer", "sdp": answer, "session": session}
                        )
                    else:
                        resp = json.dumps({"type": "error"})
                    writer.write(resp.encode() + b"\n")
                    await writer.drain()
                elif msg.get("type") in ("add-ice-candidate", "candidate"):
                    resp = worker.add_ice_candidate(
                        msg.get("session"),
                        msg.get("candidate", ""),
                        msg.get("sdpMLineIndex", 0),
                    )
                    writer.write(json.dumps(resp).encode() + b"\n")
                    await writer.drain()
                elif msg.get("type") == "bitrate":
                    resp = worker.set_bitrate(msg.get("bitrate"))
                    writer.write(json.dumps(resp).encode() + b"\n")
                    await writer.drain()
        except Exception as e:
            print(f"Client error: {e}", flush=True)

    server = await asyncio.start_unix_server(handle_client, args.sock)
    os.chmod(args.sock, 0o777)
    print(f"GStreamer WebRTC worker ready on {args.sock}", flush=True)

    await asyncio.Event().wait()  # run forever


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
