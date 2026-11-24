#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Live WebRTC client for Gate 2 validation.

This is a real GStreamer ``webrtcbin`` peer, not a synthetic route probe.  It
creates a receive-only offer (H.264 video, optionally Opus audio), POSTs it to
an already-running node's ``/webrtc/offer``, applies the answer and its ICE
candidates, and counts decoded frames.  It can also POST ``/webrtc/bitrate``
once media is flowing.

Requirements: system Python 3 with ``gi`` and the GStreamer plugins
``webrtcbin``, ``nice``, ``dtls``, ``rtph264depay``, ``avdec_h264``; add
``rtpopusdepay``/``opusdec`` for ``--audio``.  Nothing is installed by this
script.

Example:

    python3 scripts/gate2/webrtc-client.py \
      --node-url http://127.0.0.1:7382 --duration 8 --audio \
      --bitrate 4000000 --report-json /tmp/gate2-webrtc.json

Exit status: 0 when ICE/DTLS connected and at least one decoded video buffer
arrived; 1 on negotiation/media failure; 2 on usage or environment failure.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request

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
    "payload=97,clock-rate=48000"
)


class Client:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.node = args.node_url.rstrip("/")
        self.offer_url = (
            args.offer_url or (self.node + "/webrtc/offer")
        ).rstrip()
        self.bitrate_url = (
            args.bitrate_url or (self.node + "/webrtc/bitrate")
        ).rstrip()
        self.pipeline = Gst.Pipeline.new("gate2-webrtc-client")
        self.webrtc = Gst.ElementFactory.make("webrtcbin", "webrtc")
        if self.webrtc is None:
            raise SystemExit("webrtcbin is not available")
        self.webrtc.set_property("bundle-policy", "max-bundle")
        self.pipeline.add(self.webrtc)

        self.media = {
            "video": {"buffers": 0, "bytes": 0, "caps": None},
            "audio": {"buffers": 0, "bytes": 0, "caps": None},
        }
        self.elements: list[Gst.Element] = []
        self.connection_state = None
        self.ice_connection_state = None
        self.ice_gathering_state = None
        self.local_candidates: list[tuple[int, str]] = []
        self.offer_sdp = None
        self.answer_sdp = None
        self.answer = None
        self.offer_sent = False
        self.connection_deadline = time.monotonic() + args.connect_timeout
        self.media_deadline = None
        self.bitrate_result = None
        self.failure = None
        self.loop = GLib.MainLoop()
        self.finished = False

        self.webrtc.connect("on-negotiation-needed", self.on_negotiation_needed)
        self.webrtc.connect("pad-added", self.on_pad_added)
        self.webrtc.connect("on-ice-candidate", self.on_ice_candidate)
        self.webrtc.connect(
            "notify::ice-gathering-state", self.on_gathering_state
        )
        self.webrtc.connect(
            "notify::connection-state", self.on_connection_state
        )
        self.webrtc.connect(
            "notify::ice-connection-state", self.on_ice_connection_state
        )

    # -- offer/answer ----------------------------------------------------

    def add_transceivers(self) -> None:
        video_caps = Gst.Caps.from_string(VIDEO_CAPS)
        self.webrtc.emit(
            "add-transceiver",
            GstWebRTC.WebRTCRTPTransceiverDirection.RECVONLY,
            video_caps,
        )
        if self.args.audio:
            audio_caps = Gst.Caps.from_string(AUDIO_CAPS)
            self.webrtc.emit(
                "add-transceiver",
                GstWebRTC.WebRTCRTPTransceiverDirection.RECVONLY,
                audio_caps,
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
        # Keep the SDP text immediately; the GstSdp object is not safe to hold
        # beyond the callback.
        self.offer_sdp = offer.sdp.as_text()
        self.webrtc.emit("set-local-description", offer, Gst.Promise.new())
        if self.ice_gathering_state == GstWebRTC.WebRTCICEGatheringState.COMPLETE:
            GLib.idle_add(self.send_offer)
        # Otherwise `on_gathering_state` sends it.

    def on_gathering_state(self, _webrtc, _pspec) -> None:
        self.ice_gathering_state = self.webrtc.get_property(
            "ice-gathering-state"
        )
        if (
            not self.offer_sent
            and self.offer_sdp
            and self.ice_gathering_state
            == GstWebRTC.WebRTCICEGatheringState.COMPLETE
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
        answer = post_json(
            self.offer_url,
            {"sdp": sdp, "type": "offer"},
            timeout=self.args.http_timeout,
        )
        if answer.get("type") != "answer" or not answer.get("sdp"):
            self.fail(f"/webrtc/offer returned {answer!r}")
            return False
        self.answer = answer
        self.answer_sdp = answer["sdp"]
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
        self.webrtc.emit(
            "set-remote-description", description, Gst.Promise.new()
        )
        # The worker puts candidates in the answer SDP.  Add them explicitly
        # too so this client does not depend on webrtcbin's SDP parsing.
        for mline, candidate in remote_ice_candidates(self.answer_sdp):
            self.webrtc.emit("add-ice-candidate", mline, candidate)
        GLib.timeout_add(100, self.check_connection)
        return False

    # -- media -----------------------------------------------------------

    def attach_media(self, pad) -> None:
        caps = pad.get_current_caps() or pad.query_caps(None)
        text = caps.to_string() if caps else ""
        if "media=(string)video" in text or text.startswith("application/x-rtp") and "H264" in text:
            kind = "video"
            chain = [
                ("rtph264depay", None),
                ("h264parse", None),
                ("avdec_h264", None),
                ("videoconvert", None),
            ]
        elif "media=(string)audio" in text or "OPUS" in text:
            kind = "audio"
            chain = [
                ("rtpopusdepay", None),
                ("opusdec", None),
                ("audioconvert", None),
                ("audioresample", None),
            ]
        else:
            return

        src_pad = pad
        for factory, _unused in chain:
            element = Gst.ElementFactory.make(factory, None)
            if element is None:
                print(
                    f"missing GStreamer element {factory}; not attaching {kind}",
                    file=sys.stderr,
                )
                return
            self.pipeline.add(element)
            self.elements.append(element)
            if src_pad.link(element.get_static_pad("sink")) != Gst.PadLinkReturn.OK:
                print(
                    f"could not link {kind} chain at {factory}",
                    file=sys.stderr,
                )
                return
            src_pad = element.get_static_pad("src")
        sink = Gst.ElementFactory.make("fakesink", f"{kind}-sink")
        sink.set_property("sync", False)
        sink.set_property("signal-handoffs", True)
        self.pipeline.add(sink)
        self.elements.append(sink)
        if src_pad.link(sink.get_static_pad("sink")) != Gst.PadLinkReturn.OK:
            print(f"could not link {kind} chain to fakesink", file=sys.stderr)
            return

        def on_buffer(_sink, buffer, _pad, kind=kind):
            entry = self.media[kind]
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
        self.attach_media(pad)

    # -- state -----------------------------------------------------------

    def on_connection_state(self, _webrtc, _pspec) -> None:
        self.connection_state = self.webrtc.get_property("connection-state")

    def on_ice_connection_state(self, _webrtc, _pspec) -> None:
        self.ice_connection_state = self.webrtc.get_property(
            "ice-connection-state"
        )

    def check_connection(self) -> bool:
        if self.finished:
            return False
        connected = (
            self.connection_state
            == GstWebRTC.WebRTCPeerConnectionState.CONNECTED
        )
        failed = self.connection_state in (
            GstWebRTC.WebRTCPeerConnectionState.FAILED,
            GstWebRTC.WebRTCPeerConnectionState.CLOSED,
        )
        if failed:
            self.fail(
                "WebRTC connection state is "
                f"{self.connection_state_name()}"
            )
            return False
        if connected and self.media_deadline is None:
            self.media_deadline = time.monotonic() + self.args.duration
            if self.args.bitrate is not None:
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
            body = post_json(
                self.bitrate_url,
                {"bitrate": self.args.bitrate},
                timeout=self.args.http_timeout,
            )
            self.bitrate_result = {"ok": True, "body": body}
        except RuntimeError as error:
            self.bitrate_result = {"ok": False, "error": str(error)}

    def connection_state_name(self) -> str:
        return state_name(
            GstWebRTC.WebRTCPeerConnectionState, self.connection_state
        )

    def ice_state_name(self) -> str:
        return state_name(
            GstWebRTC.WebRTCICEConnectionState, self.ice_connection_state
        )

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

    def run(self) -> int:
        self.add_transceivers()
        self.pipeline.set_state(Gst.State.PLAYING)
        GLib.timeout_add(200, self.check_connection)
        # Hard stop in case negotiation stalls before any deadline fires.
        GLib.timeout_add_seconds(
            int(self.args.connect_timeout + self.args.duration + 10),
            lambda: (self.fail("client timed out"), False)[-1],
        )
        self.loop.run()

        summary = {
            "ok": self.failure is None
            and self.media["video"]["buffers"] > 0,
            "node_url": self.node,
            "failure": self.failure,
            "offer_sent": self.offer_sent,
            "answer_type": self.answer.get("type") if self.answer else None,
            "offer_candidates": len(self.local_candidates),
            "answer_candidates": len(remote_ice_candidates(self.answer_sdp or "")),
            "connection_state": self.connection_state_name(),
            "ice_connection_state": self.ice_state_name(),
            "ice_gathering_state": state_name(
                GstWebRTC.WebRTCICEGatheringState, self.ice_gathering_state
            ),
            "media": self.media,
            "bitrate": self.bitrate_result,
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
        return 0 if summary["ok"] else 1


# -- SDP helpers (mirror softnode/gst_webrtc_worker.py) ------------------


def split_sdp_media_sections(sdp: str) -> tuple[list[str], list[list[str]]]:
    preamble: list[str] = []
    sections: list[list[str]] = []
    current = None
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
    _, sections = split_sdp_media_sections(sdp)
    candidates = []
    for index, lines in enumerate(sections):
        for line in lines:
            if line.startswith("a=candidate:"):
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


def post_json(url: str, payload: dict, timeout: float) -> dict:
    data = json.dumps(payload).encode()
    request = urllib.request.Request(
        url, data=data, headers={"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read()
            status = response.status
    except urllib.error.HTTPError as error:
        body = error.read()
        raise RuntimeError(
            f"HTTP {error.code} from {url}: {body.decode(errors='replace')}"
        ) from error
    except urllib.error.URLError as error:
        raise RuntimeError(f"could not reach {url}: {error.reason}") from error
    if status != 200:
        raise RuntimeError(
            f"HTTP {status} from {url}: {body.decode(errors='replace')}"
        )
    try:
        return json.loads(body)
    except json.JSONDecodeError as error:
        raise RuntimeError(f"non-JSON response from {url}: {body!r}") from error


def state_name(enum, value) -> str:
    if value is None:
        return "unknown"
    try:
        return enum(value).value_nick
    except Exception:
        return str(value)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Live GStreamer WebRTC client for a running ozma-node"
    )
    parser.add_argument("--node-url", required=True)
    parser.add_argument(
        "--offer-url",
        default=None,
        help=(
            "override the offer endpoint (default: NODE_URL/webrtc/offer); "
            "use for the controller proxy "
            "/api/v1/streams/<node-id>/webrtc"
        ),
    )
    parser.add_argument(
        "--bitrate-url",
        default=None,
        help="override the bitrate endpoint (default: NODE_URL/webrtc/bitrate)",
    )
    parser.add_argument(
        "--audio",
        action="store_true",
        help="also offer a recvonly Opus transceiver",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=8.0,
        help="seconds of media to count after ICE/DTLS connects",
    )
    parser.add_argument(
        "--connect-timeout",
        type=float,
        default=15.0,
        help="seconds to wait for ICE/DTLS connection",
    )
    parser.add_argument(
        "--http-timeout", type=float, default=20.0, help="HTTP timeout"
    )
    parser.add_argument(
        "--bitrate",
        type=int,
        default=None,
        help="optional bitrate in bps to POST once media is flowing",
    )
    parser.add_argument("--offer-out", default=None)
    parser.add_argument("--answer-out", default=None)
    parser.add_argument("--report-json", default=None)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    client = Client(args)
    return client.run()


if __name__ == "__main__":
    raise SystemExit(main())
