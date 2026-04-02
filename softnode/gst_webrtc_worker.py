#!/usr/bin/env python3
"""
GStreamer WebRTC worker — runs under system Python with GStreamer bindings.

Reads raw BGRA framebuffer from shared memory, encodes with NVENC,
serves via webrtcbin. Signaling via Unix socket.
"""

import argparse
import asyncio
import json
import os
import struct
import sys
import time
import threading

import gi
gi.require_version('Gst', '1.0')
gi.require_version('GstWebRTC', '1.0')
gi.require_version('GstSdp', '1.0')
from gi.repository import Gst, GstWebRTC, GstSdp, GLib

Gst.init(None)


class WebRTCWorker:
    def __init__(self, shm_path: str, width: int, height: int, fps: int):
        self.shm_path = shm_path
        self.width = width
        self.height = height
        self.fps = fps
        self.sessions = {}
        self.mainloop = GLib.MainLoop()

    def create_session(self, offer_sdp: str) -> str | None:
        """Create a WebRTC session from an SDP offer. Returns answer SDP."""
        import uuid
        sid = uuid.uuid4().hex[:8]

        nvenc = Gst.ElementFactory.find("nvh264enc")
        if nvenc:
            enc = "nvh264enc preset=low-latency-hq rc-mode=cbr bitrate=4000 zerolatency=true"
        else:
            enc = "x264enc tune=zerolatency speed-preset=ultrafast bitrate=3000 key-int-max=30"

        pipe_str = (
            f"appsrc name=src format=time is-live=true do-timestamp=true "
            f"caps=video/x-raw,format=BGRA,width={self.width},height={self.height},"
            f"framerate={self.fps}/1 "
            f"! videoconvert "
            f"! {enc} "
            f"! video/x-h264,profile=baseline "
            f"! rtph264pay config-interval=-1 pt=96 "
            f"! webrtcbin name=webrtc bundle-policy=max-bundle"
        )

        pipeline = Gst.parse_launch(pipe_str)
        webrtc = pipeline.get_by_name("webrtc")
        appsrc = pipeline.get_by_name("src")

        # Set offer
        _, sdpmsg = GstSdp.SDPMessage.new_from_text(offer_sdp)
        offer = GstWebRTC.WebRTCSessionDescription.new(
            GstWebRTC.WebRTCSDPType.OFFER, sdpmsg)
        webrtc.emit("set-remote-description", offer, None)

        # Create answer
        answer_sdp = [None]
        event = threading.Event()

        def on_answer(promise):
            reply = promise.get_reply()
            if reply:
                answer = reply.get_value("answer")
                if answer:
                    webrtc.emit("set-local-description", answer, None)
                    answer_sdp[0] = answer.sdp.as_text()
            event.set()

        promise = Gst.Promise.new_with_change_func(on_answer)
        webrtc.emit("create-answer", None, promise)
        event.wait(timeout=5)

        if not answer_sdp[0]:
            pipeline.set_state(Gst.State.NULL)
            return None

        pipeline.set_state(Gst.State.PLAYING)

        self.sessions[sid] = {"pipeline": pipeline, "appsrc": appsrc}

        # Start frame feeder
        t = threading.Thread(target=self._feed, args=(sid, appsrc), daemon=True)
        t.start()

        enc_name = "NVENC" if nvenc else "x264"
        print(f"Session {sid}: {self.width}x{self.height}@{self.fps} {enc_name}", flush=True)
        return answer_sdp[0]

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
    parser = argparse.ArgumentParser()
    parser.add_argument("--sock", required=True)
    parser.add_argument("--shm", required=True)
    parser.add_argument("--width", type=int, default=640)
    parser.add_argument("--height", type=int, default=480)
    parser.add_argument("--fps", type=int, default=30)
    args = parser.parse_args()

    worker = WebRTCWorker(args.shm, args.width, args.height, args.fps)

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
                    answer = worker.create_session(msg["sdp"])
                    resp = json.dumps({"type": "answer", "sdp": answer} if answer
                                      else {"type": "error"})
                    writer.write(resp.encode() + b"\n")
                    await writer.drain()
        except Exception as e:
            print(f"Client error: {e}", flush=True)

    server = await asyncio.start_unix_server(handle_client, args.sock)
    os.chmod(args.sock, 0o777)
    print(f"GStreamer WebRTC worker ready on {args.sock}", flush=True)

    await asyncio.Event().wait()  # run forever


if __name__ == "__main__":
    asyncio.run(main())
