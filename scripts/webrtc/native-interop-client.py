#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""Live interop peer for the native Rust WebRTC helper.

This is a real GStreamer ``webrtcbin`` receive-only peer, not a probe. It:

1. writes an animated packed-BGRA test pattern to the same shared-memory file
   the node writes (atomic temp-file + rename, like the Rust node);
2. creates a receive-only H.264 offer and gathers ICE candidates;
3. sends ``{"type":"offer",...}`` over the helper's Unix socket;
4. applies the answer and its candidates, then adds one of its own candidates
   through ``add-ice-candidate`` to exercise the trickle path;
5. counts decoded I420 buffers and records their caps;
6. asks for a 2.5 Mbps bitrate over signaling and checks the ack.

Exit 0 only when ICE/DTLS connected, at least ``--min-frames`` decoded buffers
arrived, and the decoded width/height match ``--width``/``--height``.

Usage:
  python3 scripts/webrtc/native-interop-client.py \
      --sock /tmp/ozma-native.sock --shm /tmp/ozma-native.bgra \
      --width 640 --height 480 --fps 30 --duration 6 \
      --report-json /tmp/ozma-native-interop.json
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import sys
import threading
import time

try:
    import gi

    gi.require_version("Gst", "1.0")
    gi.require_version("GstWebRTC", "1.0")
    gi.require_version("GstSdp", "1.0")
    from gi.repository import GLib, Gst, GstSdp, GstWebRTC
except (ImportError, ValueError) as error:  # pragma: no cover - host dependent
    print(f"GStreamer Python bindings are unavailable: {error}", file=sys.stderr)
    raise SystemExit(2)

Gst.init(None)

VIDEO_CAPS = (
    "application/x-rtp,media=video,encoding-name=H264,"
    "payload=96,clock-rate=90000"
)
AUDIO_CAPS = (
    "application/x-rtp,media=audio,encoding-name=OPUS,"
    "payload={pt},clock-rate=48000,channels=2"
)
DESCRIPTION_TIMEOUT_SECONDS = 10.0


# ---------------------------------------------------------------------------
# Shared-memory frame feeder
# ---------------------------------------------------------------------------


class FrameFeeder(threading.Thread):
    """Write a real animated BGRA frame to the helper's shared-memory file."""

    def __init__(self, path: str, width: int, height: int, fps: int) -> None:
        super().__init__(daemon=True, name="bgra-feeder")
        self.path = path
        self.width = width
        self.height = height
        self.fps = max(1, fps)
        self.stop_event = threading.Event()
        self.frames_written = 0
        self._base = self._make_base()

    def _make_base(self) -> bytearray:
        width, height = self.width, self.height
        frame = bytearray(width * height * 4)
        for y in range(height):
            row = y * width * 4
            for x in range(width):
                offset = row + x * 4
                frame[offset] = (x * 255) // max(1, width - 1)        # B
                frame[offset + 1] = (y * 255) // max(1, height - 1)    # G
                frame[offset + 2] = ((x + y) * 255) // max(1, width + height - 2)  # R
                frame[offset + 3] = 0xFF                               # A
        return frame

    def _frame(self, phase: int) -> bytearray:
        frame = bytearray(self._base)
        bar_width = max(8, self.width // 16)
        x0 = (phase * 7) % max(1, self.width - bar_width)
        for y in range(self.height):
            row = y * self.width * 4
            start = row + x0 * 4
            end = start + bar_width * 4
            frame[start:end] = b"\xff\xff\xff\xff" * bar_width
        return frame

    def run(self) -> None:
        interval = 1.0 / self.fps
        tmp = self.path + ".tmp"
        phase = 0
        deadline = time.monotonic()
        while not self.stop_event.is_set():
            frame = self._frame(phase)
            try:
                with open(tmp, "wb") as handle:
                    handle.write(frame)
                os.replace(tmp, self.path)
                self.frames_written += 1
            except OSError as error:
                print(f"frame feeder write failed: {error}", file=sys.stderr)
                return
            phase += 1
            deadline += interval
            delay = deadline - time.monotonic()
            if delay > 0:
                time.sleep(delay)
            else:
                deadline = time.monotonic()

    def stop(self) -> None:
        self.stop_event.set()


# ---------------------------------------------------------------------------
# Signaling socket
# ---------------------------------------------------------------------------


class SignalSocket:
    def __init__(self, path: str, timeout: float) -> None:
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        self.sock.connect(path)
        self.reader = self.sock.makefile("rb")

    def send(self, message: dict) -> None:
        payload = json.dumps(message).encode() + b"\n"
        self.sock.sendall(payload)

    def receive(self) -> dict:
        line = self.reader.readline()
        if not line:
            raise RuntimeError("helper closed the signaling socket")
        return json.loads(line)

    def close(self) -> None:
        try:
            self.reader.close()
        finally:
            self.sock.close()


# ---------------------------------------------------------------------------
# SDP helpers
# ---------------------------------------------------------------------------


def split_sdp_media_sections(sdp: str):
    preamble: list[str] = []
    sections: list[list[str]] = []
    for raw_line in sdp.splitlines():
        line = raw_line.rstrip("\r")
        if line.startswith("m="):
            sections.append([line])
        elif sections:
            sections[-1].append(line)
        else:
            preamble.append(line)
    return preamble, sections


def remote_ice_candidates(sdp: str) -> list[tuple[int, str]]:
    _, sections = split_sdp_media_sections(sdp)
    candidates = []
    for index, lines in enumerate(sections):
        for line in lines:
            if not line.startswith("a=candidate:"):
                continue
            value = line[len("a=") :].strip()
            if value and not value.startswith("candidate:end-of-candidates"):
                candidates.append((index, value))
    return candidates


def append_ice_candidates(sdp: str, candidates: list[tuple[int, str]]) -> str:
    if not candidates:
        return sdp
    preamble, sections = split_sdp_media_sections(sdp)
    if not sections:
        return sdp
    by_index: dict[int, list[str]] = {}
    for index, candidate in candidates:
        by_index.setdefault(index, []).append(candidate)
    lines = list(preamble)
    for index, section in enumerate(sections):
        existing = {line for line in section if line.startswith("a=candidate:")}
        additions = [
            "a=" + candidate
            for candidate in by_index.get(index, [])
            if "a=" + candidate not in existing
        ]
        if additions:
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


def state_name(enum, value) -> str:
    if value is None:
        return "unknown"
    try:
        return enum(value).value_nick
    except Exception:
        return str(value)


# ---------------------------------------------------------------------------
# Client
# ---------------------------------------------------------------------------


class Client:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.media = {"video": {"buffers": 0, "bytes": 0, "caps": None}}
        self.elements: list[Gst.Element] = []
        self.connection_state = None
        self.ice_connection_state = None
        self.ice_gathering_state = None
        self.local_candidates: list[tuple[int, str]] = []
        self.offer_sdp = None
        self.answer_sdp = None
        self.answer = None
        self.session = None
        self.offer_sent = False
        self.trickle_ack = None
        self.bitrate_ack = None
        self.failure = None
        self.finished = False
        self.connection_deadline = time.monotonic() + args.connect_timeout
        self.media_deadline = None
        self.loop = GLib.MainLoop()
        self.signals = SignalSocket(args.sock, args.http_timeout)

        self.pipeline = Gst.Pipeline.new("native-interop-client")
        self.webrtc = Gst.ElementFactory.make("webrtcbin", "webrtc")
        if self.webrtc is None:
            raise SystemExit("webrtcbin is not available")
        self.webrtc.set_property("bundle-policy", "max-bundle")
        self.pipeline.add(self.webrtc)

        self.webrtc.connect("on-negotiation-needed", self.on_negotiation_needed)
        self.webrtc.connect("pad-added", self.on_pad_added)
        self.webrtc.connect("on-ice-candidate", self.on_ice_candidate)
        self.webrtc.connect("notify::ice-gathering-state", self.on_gathering_state)
        self.webrtc.connect("notify::connection-state", self.on_connection_state)
        self.webrtc.connect("notify::ice-connection-state", self.on_ice_connection_state)

    # -- negotiation -----------------------------------------------------

    def add_transceivers(self) -> None:
        caps = Gst.Caps.from_string(VIDEO_CAPS)
        self.webrtc.emit(
            "add-transceiver",
            GstWebRTC.WebRTCRTPTransceiverDirection.RECVONLY,
            caps,
        )
        if self.args.audio:
            self.webrtc.emit(
                "add-transceiver",
                GstWebRTC.WebRTCRTPTransceiverDirection.RECVONLY,
                Gst.Caps.from_string(AUDIO_CAPS.format(pt=self.args.audio_pt)),
            )

    def on_negotiation_needed(self, _webrtc) -> None:
        self.webrtc.emit(
            "create-offer",
            None,
            Gst.Promise.new_with_change_func(self.on_offer_created),
        )

    def on_offer_created(self, promise) -> None:
        reply = promise.get_reply()
        offer = reply.get_value("offer") if reply else None
        if offer is None:
            self.fail("create-offer returned no offer")
            return
        self.offer_sdp = offer.sdp.as_text()
        self.webrtc.emit("set-local-description", offer, Gst.Promise.new())
        if self.ice_gathering_state == GstWebRTC.WebRTCICEGatheringState.COMPLETE:
            GLib.idle_add(self.send_offer)

    def on_gathering_state(self, _webrtc, _pspec) -> None:
        self.ice_gathering_state = self.webrtc.get_property("ice-gathering-state")
        if (
            not self.offer_sent
            and self.offer_sdp
            and self.ice_gathering_state == GstWebRTC.WebRTCICEGatheringState.COMPLETE
        ):
            GLib.idle_add(self.send_offer)

    def on_ice_candidate(self, _webrtc, mline, candidate) -> None:
        if candidate:
            self.local_candidates.append((int(mline), candidate))

    def send_offer(self) -> bool:
        if self.offer_sent:
            return False
        self.offer_sent = True
        sdp = append_ice_candidates(self.offer_sdp, self.local_candidates)
        if self.args.offer_out:
            with open(self.args.offer_out, "w", encoding="utf-8") as handle:
                handle.write(sdp)
        try:
            self.signals.send({"type": "offer", "sdp": sdp})
            answer = self.signals.receive()
        except (OSError, RuntimeError, json.JSONDecodeError) as error:
            self.fail(f"offer exchange failed: {error}")
            return False

        if answer.get("type") != "answer" or not answer.get("sdp"):
            self.fail(f"helper returned {answer!r}")
            return False
        self.answer = answer
        self.answer_sdp = answer["sdp"]
        self.session = answer.get("session")
        if self.args.answer_out:
            with open(self.args.answer_out, "w", encoding="utf-8") as handle:
                handle.write(self.answer_sdp)

        result, message = GstSdp.SDPMessage.new_from_text(self.answer_sdp)
        if result != GstSdp.SDPResult.OK:
            self.fail(f"could not parse the answer SDP: {result}")
            return False
        description = GstWebRTC.WebRTCSessionDescription.new(
            GstWebRTC.WebRTCSDPType.ANSWER, message
        )
        self.webrtc.emit("set-remote-description", description, Gst.Promise.new())
        for mline, candidate in remote_ice_candidates(self.answer_sdp):
            self.webrtc.emit("add-ice-candidate", mline, candidate)

        # Exercise the helper's trickle path with a real local candidate.
        if self.local_candidates:
            mline, candidate = self.local_candidates[0]
            try:
                self.signals.send(
                    {
                        "type": "add-ice-candidate",
                        "session": self.session,
                        "candidate": candidate,
                        "sdpMLineIndex": mline,
                    }
                )
                self.trickle_ack = self.signals.receive()
            except (OSError, RuntimeError, json.JSONDecodeError) as error:
                self.trickle_ack = {"ok": False, "error": str(error)}

        GLib.timeout_add(200, self.check_connection)
        return False

    # -- media -----------------------------------------------------------

    def attach_video(self, pad) -> None:
        caps = pad.get_current_caps() or pad.query_caps(None)
        text = caps.to_string() if caps else ""
        if "H264" not in text:
            return
        src_pad = pad
        for factory in ("rtph264depay", "h264parse", "avdec_h264", "videoconvert"):
            element = Gst.ElementFactory.make(factory, None)
            if element is None:
                print(f"missing GStreamer element {factory}", file=sys.stderr)
                return
            self.pipeline.add(element)
            self.elements.append(element)
            if src_pad.link(element.get_static_pad("sink")) != Gst.PadLinkReturn.OK:
                print(f"could not link H.264 chain at {factory}", file=sys.stderr)
                return
            src_pad = element.get_static_pad("src")

        sink = Gst.ElementFactory.make("fakesink", "video-sink")
        sink.set_property("sync", False)
        sink.set_property("signal-handoffs", True)
        self.pipeline.add(sink)
        self.elements.append(sink)
        if src_pad.link(sink.get_static_pad("sink")) != Gst.PadLinkReturn.OK:
            print("could not link the H.264 chain to fakesink", file=sys.stderr)
            return

        def on_buffer(_sink, buffer, _pad):
            entry = self.media["video"]
            entry["buffers"] += 1
            entry["bytes"] += buffer.get_size()
            if entry["caps"] is None:
                sink_pad = _sink.get_static_pad("sink")
                current = sink_pad.get_current_caps()
                entry["caps"] = current.to_string() if current else None

        sink.connect("handoff", on_buffer)
        for element in self.elements:
            if element.get_parent() is self.pipeline:
                element.sync_state_with_parent()

    def on_pad_added(self, _webrtc, pad) -> None:
        self.attach_video(pad)

    # -- state -----------------------------------------------------------

    def on_connection_state(self, _webrtc, _pspec) -> None:
        self.connection_state = self.webrtc.get_property("connection-state")

    def on_ice_connection_state(self, _webrtc, _pspec) -> None:
        self.ice_connection_state = self.webrtc.get_property("ice-connection-state")

    def check_connection(self) -> bool:
        if self.finished:
            return False
        connected = (
            self.connection_state == GstWebRTC.WebRTCPeerConnectionState.CONNECTED
        )
        failed = self.connection_state in (
            GstWebRTC.WebRTCPeerConnectionState.FAILED,
            GstWebRTC.WebRTCPeerConnectionState.CLOSED,
        )
        if failed:
            self.fail(f"connection state is {self.connection_state_name()}")
            return False
        if connected and self.media_deadline is None:
            self.media_deadline = time.monotonic() + self.args.duration
            self.request_bitrate()
        if self.media_deadline is not None:
            if time.monotonic() >= self.media_deadline:
                self.finish()
                return False
        elif time.monotonic() >= self.connection_deadline:
            self.fail(
                "ICE/DTLS did not connect within "
                f"{self.args.connect_timeout}s "
                f"(connection={self.connection_state_name()}, "
                f"ice={self.ice_state_name()})"
            )
            return False
        return True

    def request_bitrate(self) -> None:
        try:
            self.signals.send({"type": "bitrate", "bitrate": self.args.bitrate})
            self.bitrate_ack = self.signals.receive()
        except (OSError, RuntimeError, json.JSONDecodeError) as error:
            self.bitrate_ack = {"ok": False, "error": str(error)}

    def connection_state_name(self) -> str:
        return state_name(GstWebRTC.WebRTCPeerConnectionState, self.connection_state)

    def ice_state_name(self) -> str:
        return state_name(GstWebRTC.WebRTCICEConnectionState, self.ice_connection_state)

    def fail(self, message: str) -> None:
        if self.failure is None:
            self.failure = message
        self.finish()

    def finish(self) -> None:
        if self.finished:
            return
        self.finished = True
        self.pipeline.set_state(Gst.State.NULL)
        self.loop.quit()

    def decoded_dimensions(self):
        caps = self.media["video"]["caps"] or ""
        width = re.search(r"width=(?:\(int\))?(\d+)", caps)
        height = re.search(r"height=(?:\(int\))?(\d+)", caps)
        return (
            int(width.group(1)) if width else None,
            int(height.group(1)) if height else None,
        )

    def run(self) -> int:
        self.add_transceivers()
        self.pipeline.set_state(Gst.State.PLAYING)
        GLib.timeout_add(200, self.check_connection)
        GLib.timeout_add_seconds(
            int(self.args.connect_timeout + self.args.duration + 10),
            lambda: (self.fail("client timed out"), False)[-1],
        )
        self.loop.run()

        width, height = self.decoded_dimensions()
        buffers = self.media["video"]["buffers"]
        dimensions_ok = width == self.args.width and height == self.args.height
        ok = (
            self.failure is None
            and buffers >= self.args.min_frames
            and dimensions_ok
            and (self.trickle_ack or {}).get("ok") is True
        )
        summary = {
            "ok": ok,
            "failure": self.failure,
            "socket": self.args.sock,
            "shm": self.args.shm,
            "feeder_frames": self.feeder.frames_written,
            "offer_sent": self.offer_sent,
            "audio_offered": bool(self.args.audio),
            "answer_type": self.answer.get("type") if self.answer else None,
            "session": self.session,
            "offer_candidates": len(self.local_candidates),
            "answer_candidates": len(remote_ice_candidates(self.answer_sdp or "")),
            "connection_state": self.connection_state_name(),
            "ice_connection_state": self.ice_state_name(),
            "ice_gathering_state": state_name(
                GstWebRTC.WebRTCICEGatheringState, self.ice_gathering_state
            ),
            "decoded_video": self.media["video"],
            "expected_dimensions": [self.args.width, self.args.height],
            "decoded_dimensions": [width, height],
            "trickle": self.trickle_ack,
            "bitrate": self.bitrate_ack,
            "sdp": {
                "offer_lines": len((self.offer_sdp or "").splitlines()),
                "answer_lines": len((self.answer_sdp or "").splitlines()),
            },
        }
        print(json.dumps(summary, indent=2, default=str))
        if self.args.report_json:
            with open(self.args.report_json, "w", encoding="utf-8") as handle:
                json.dump(summary, handle, indent=2, default=str)
                handle.write("\n")
        return 0 if ok else 1


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Live GStreamer peer for the native Rust WebRTC helper"
    )
    parser.add_argument("--sock", required=True, help="helper signaling Unix socket")
    parser.add_argument("--shm", required=True, help="helper BGRA shared-memory file")
    parser.add_argument("--width", type=int, default=640)
    parser.add_argument("--height", type=int, default=480)
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument(
        "--audio",
        action="store_true",
        help="also offer a recvonly Opus m-line (the native helper answers it inactive)",
    )
    parser.add_argument(
        "--audio-pt",
        type=int,
        default=111,
        help="payload type used by the offered Opus m-line (GStreamer clients often use 97)",
    )
    parser.add_argument("--duration", type=float, default=6.0)
    parser.add_argument("--connect-timeout", type=float, default=15.0)
    parser.add_argument("--http-timeout", type=float, default=20.0)
    parser.add_argument("--bitrate", type=int, default=2_500_000)
    parser.add_argument("--min-frames", type=int, default=30)
    parser.add_argument("--offer-out", default=None)
    parser.add_argument("--answer-out", default=None)
    parser.add_argument("--report-json", default=None)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    client = Client(args)
    client.feeder = FrameFeeder(args.shm, args.width, args.height, args.fps)
    client.feeder.start()
    try:
        return client.run()
    finally:
        client.feeder.stop()
        try:
            client.signals.close()
        except OSError:
            pass
        client.feeder.join(timeout=2)


if __name__ == "__main__":
    raise SystemExit(main())
