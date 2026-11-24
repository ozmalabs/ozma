# Gate 2 live WebRTC report

**Scope:** the Rust node's supervised GStreamer/Python WebRTC helper, exercised
end to end against a disposable QMP display source: real SDP offer/answer, ICE
and DTLS, H.264 video, Opus audio, and `/webrtc/bitrate`.

**Result: PASS for the helper path.** A real GStreamer `webrtcbin` client
completed ICE/DTLS with the node, decoded **302 H.264 frames at 720x400** and
**1020 Opus buffers at 48 kHz stereo**, and `/webrtc/bitrate` acknowledged
4 Mbps. This is **not** a native-WebRTC or browser-matrix pass: the peer was
GStreamer, not a browser, and the node still runs the external Python helper.

> No package install, sudo, controller change, VM definition, or commit was
> used. QEMU was a direct headless process with no disk or network device. All
> processes, sockets, shm files, the null sink, and the temp directory were
> removed; see [Cleanup](#8-cleanup).

> **Follow-up (2026-11-24):** the controller proxy and a headless Chromium
> dashboard peer were validated later in
> [`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).
> That run also fixed the worker's hard-coded H.264/Opus payload types (Chrome
> uses 103/111, not 96/97), which this GStreamer-only report did not expose.
>
> **Follow-up (2026-11-24, browser matrix):** Playwright Firefox 159.0 with
> OpenH264 2.6.0 and Chromium 159 iPhone emulation later decoded the helper's
> H.264/Opus through the dashboard. STUN/TURN and trickle ICE remain
> unimplemented; no distinct `srflx` or relay candidate was observed. See
> [`gate2-live-webrtc-matrix-report.md`](gate2-live-webrtc-matrix-report.md).

| Item | Value |
|---|---|
| Date | 2026-11-24, ~14:13–14:26 AEDT (03:13–03:26Z), final rerun at 14:26 |
| Git HEAD | `cd8e8bd2408cdb0f81ba57139888efb71377c7a2` |
| Working tree | dirty with this run's fixes and concurrent edits; nothing committed |
| Python / GStreamer | Python 3.14.10, GStreamer 1.28.9 |
| QEMU | 11.1.3, TCG, 720x400 VGA, no guest |
| Encoder | x264 fallback (`x264enc`, no hardware encoder) |
| Ports | HTTP 47433, QUIC 47435, HID 60291 |
| Temp dir | `/tmp/opencode/gate2-webrtc` (removed) |
| Audio | disposable null sink `ozma-gate2-webrtc-test.monitor`, 440 Hz tone |

## 1. Client availability audit

Checked without installing anything:

| Client | Availability | Use |
|---|---|---|
| Python `aiortc` | **absent** (`ModuleNotFoundError: aiortc`; Python 3.14) | not used |
| GStreamer Python (`gi` + `GstWebRTC`) | **present**, GStreamer 1.28.9 | **used for the offer/answer and media counters** |
| `gst-launch-1.0`, `webrtcbin`, `nice`, `dtls`, `srtp` | **present** | helper and client elements |
| `x264enc`, `rtph264pay`, `avdec_h264` | **present** | H.264 fallback encode/decode |
| `opusenc`, `rtpopuspay`, `opusdec` | **present** | Opus audio |
| Playwright / geckodriver | **absent** | no browser automation |
| Firefox | installed | not automated; no browser run in this report |
| QEMU / ffmpeg | QEMU 11.1.3, ffmpeg n9.1 | QEMU source used; ffmpeg not needed |

`aiortc` was not installed. The repository script
`scripts/gate2/webrtc-client.py` is a real GStreamer peer, not an HTTP-only
probe.

## 2. Live source

A disposable, headless QEMU process provided a real QMP `screendump` display
(no guest OS). The node's WebRTC dimensions must match the screendump exactly,
so the run used 720x400.

```bash
BASE=/tmp/opencode/gate2-webrtc/live
qemu-system-x86_64 \
  -name ozma-gate2-webrtc \
  -machine pc,accel=tcg \
  -m 256 \
  -nodefaults \
  -device VGA \
  -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 \
  -display none \
  -qmp "unix:$BASE/qmp.sock,server=on,wait=off" \
  -pidfile "$BASE/qemu.pid" \
  -daemonize
```

QMP verification:

```text
greeting: QEMU 11.1.3, capabilities ["oob"]
qmp_capabilities: {"return": {}}
query-status: {"return": {"running": true, "status": "running"}}
screendump: {"return": {}}
PPM: P6 720x400 maxval=255
```

For a deterministic audio source, a disposable null sink and a 440 Hz tone
were created. The tone was captured from the sink's monitor, not from any real
output device:

```bash
pactl load-module module-null-sink sink_name=ozma-gate2-webrtc-test
gst-launch-1.0 -q audiotestsrc is-live=true wave=sine freq=440 \
  ! audioconvert ! audioresample \
  ! audio/x-raw,format=F32LE,rate=48000,channels=2 \
  ! pulsesink device=ozma-gate2-webrtc-test
```

## 3. Node command

```bash
cd /home/matt/ozma/ozma
BASE=/tmp/opencode/gate2-webrtc/live
RUST_LOG=info,ozma_node=debug ./target/debug/ozma-node \
  --name ozma-gate2-webrtc \
  --profile vm \
  --qmp-socket "$BASE/qmp.sock" \
  --input-provider qmp \
  --power-provider qmp \
  --http-port 47433 \
  --hid-udp-port 60291 \
  --cap hid,video,audio \
  --machine-class server \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$PWD/softnode/gst_webrtc_worker.py" \
  --webrtc-width 720 \
  --webrtc-height 400 \
  --webrtc-fps 15 \
  --webrtc-audio-source ozma-gate2-webrtc-test.monitor \
  --webrtc-audio-backend pulse
```

`/api/v1/connection` reported:

```json
{
  "power": {"available": true, "status": "running"},
  "input": {"sink_kind": "qmp", "available": true, "injecting": true},
  "hls": "not-configured",
  "webrtc": "running",
  "webrtc_error": null,
  "webrtc_audio_configured": true,
  "webrtc_audio_source": "ozma-gate2-webrtc-test.monitor"
}
```

The helper argv logged by the node matched the Rust contract:

```text
/usr/bin/python3 softnode/gst_webrtc_worker.py \
  --sock /tmp/ozma-webrtc-ozma-gate2-webrtc-<pid>.sock \
  --shm /dev/shm/ozma-webrtc-fb-<pid> \
  --width 720 --height 400 --fps 15 \
  --audio-source ozma-gate2-webrtc-test.monitor --audio-backend pulse
```

## 4. Real offer/answer and media

Client command:

```bash
python3 scripts/gate2/webrtc-client.py \
  --node-url http://127.0.0.1:47433 \
  --audio \
  --duration 20 \
  --bitrate 4000000 \
  --offer-out "$BASE/final-offer.sdp" \
  --answer-out "$BASE/final-answer.sdp" \
  --report-json "$BASE/final-report.json"
```

The client creates a receive-only offer (H.264 first, Opus second), waits for
ICE gathering, embeds its candidates in the offer SDP, POSTs
`/webrtc/offer`, applies the answer and its candidates, and counts decoded
buffers. Summary:

```json
{
  "ok": true,
  "answer_type": "answer",
  "offer_candidates": 9,
  "answer_candidates": 9,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "ice_gathering_state": "complete",
  "media": {
    "video": {
      "buffers": 302,
      "bytes": 130464000,
      "caps": "video/x-raw, format=I420, width=720, height=400, framerate=15/1"
    },
    "audio": {
      "buffers": 1020,
      "bytes": 3916800,
      "caps": "audio/x-raw, format=S16LE, layout=interleaved, rate=48000, channels=2"
    }
  },
  "bitrate": {"ok": true, "body": {"ok": true, "bitrate": 4000000}}
}
```

Answer SDP media shape (candidate/fingerprint values elided):

```text
m=video 9 UDP/TLS/RTP/SAVPF 96
a=mid:video0
a=setup:active
a=rtpmap:96 H264/90000
a=sendonly
a=candidate:1 1 UDP ... 192.168.40.19 <port> typ host

m=audio 9 UDP/TLS/RTP/SAVPF 97
a=mid:audio1
a=setup:active
a=rtpmap:97 OPUS/48000
a=sendonly
a=candidate:1 1 UDP ... 192.168.40.19 <port> typ host
```

Offer: 44 lines (35 base SDP plus 9 candidate lines), 9 candidates. Answer: 39
lines, 9 candidates. Both sides reported ICE `COMPLETED`; DTLS completed as
part of the peer connection `CONNECTED` state. The decoded video caps confirm real H.264 frames, not just
RTP packet receipt, and the decoded audio caps confirm real Opus frames.

A final 5-second rerun on the finished code (after the last
`a=end-of-candidates` ordering hardening) reproduced the result: connected /
ICE completed / gathering complete, **78** decoded video frames and **269**
decoded Opus buffers, with the same `{"ok":true,"bitrate":4000000}` ack.

## 5. `/webrtc/bitrate` against the live worker

While the session was live, direct route checks returned:

```text
POST /webrtc/bitrate {"bitrate":2500000}
HTTP/1.1 200 OK
{"bitrate":2500000,"ok":true}

POST /webrtc/bitrate {"bitrate":100000}
HTTP/1.1 200 OK
{"bitrate":500000,"ok":true}

POST /webrtc/bitrate {"bitrate":60000000}
HTTP/1.1 200 OK
{"bitrate":50000000,"ok":true}
```

The Rust route clamped to 500 kbps–50 Mbps, sent
`{"type":"bitrate","bitrate":<bps>}` to the helper, and only returned success
after the helper's `{"ok":true,"bitrate":<bps>}` acknowledgement. The client's
own 4 Mbps request also returned `{"ok":true,"bitrate":4000000}`.

The acknowledgement proves the worker applied the property to its live
`x264enc` session. It is not an RTCP/encoder-rate measurement.

## 6. Fixes made

1. **`softnode/gst_webrtc_worker.py` — create-answer needed a playing bin.**
   The worker applied the remote offer and called `create-answer` before the
   pipeline was in `PLAYING`; on GStreamer 1.28 that promise returned an answer
   of `NULL`, so every live offer failed. The pipeline is now set to `PLAYING`
   (and waited on) before negotiation. `set-remote-description` and
   `create-answer` now use promises; the answer is created from the remote
   description callback, not raced against it.

2. **`softnode/gst_webrtc_worker.py` — ICE candidates were never exchanged.**
   The helper ignored `on-ice-candidate`, so its answer had no `a=candidate`
   lines and the client's offer candidates were unused. The worker now:
   - extracts `a=candidate` lines from the offer and adds them with
     `add-ice-candidate`;
   - collects local candidates, waits (bounded, 5 s) for gathering to complete,
     and appends them to the answer SDP.

   This keeps the single-line JSON signaling contract unchanged.

3. **`softnode/gst_webrtc_worker.py` — Opus branch could not link.**
   The audio capsfilter forced `F32LE`, but `opusenc` accepts interleaved
   `S16LE`. The branch failed to parse and silently degraded every session to
   video-only (`a=inactive` for the audio m-line). It now uses
   `audio/x-raw,format=S16LE,rate=48000,channels=2`.

4. **Tests.** `softnode/gst_webrtc_worker_test.py` adds SDP candidate
   parse/append/idempotence tests and asserts the Opus `S16LE` format. The
   stale Rust comment that "the real Python worker ignores bitrate" was
   corrected; the no-ack test itself still asserts honest failure.

5. **New live client** `scripts/gate2/webrtc-client.py` (system GStreamer
   Python): real offer, ICE, DTLS, media counters, optional Opus and bitrate
   POST. No installs and no synthetic SDP.

## 7. Tests run

```text
python3 softnode/gst_webrtc_worker_test.py
  -> 22 passed

cargo test -p ozma-node --lib webrtc::
  -> 20 passed, 0 failed, 2 ignored

OZMA_WEBRTC_GS_SCRIPT=$PWD/softnode/gst_webrtc_worker.py \
  cargo test -p ozma-node --lib webrtc::tests::real_gstreamer_helper_starts_and_stops -- --ignored
  -> 1 passed

OZMA_WEBRTC_GS_SCRIPT=$PWD/softnode/gst_webrtc_worker.py \
OZMA_WEBRTC_GS_AUDIO_SOURCE=ozma-gate2-webrtc-test.monitor \
  cargo test -p ozma-node --lib webrtc::tests::real_gstreamer_audio_helper_starts_and_stops -- --ignored
  -> 1 passed

cargo test -p ozma-node --test parity webrtc_
  -> 4 passed

cargo test -p ozma-node --lib controller_compat::tests::webrtc_
  -> 2 passed
```

## 8. Cleanup

- Node received `SIGINT`; it reaped the helper and removed its Unix socket and
  shm file.
- QEMU was terminated with its pidfile; the QMP socket was removed with the
  temp directory.
- The tone `gst-launch` process was killed.
- The null sink was unloaded with `pactl unload-module 536870916`.
- `/tmp/opencode/gate2-webrtc` (including offer/answer/report logs) was
  removed.
- A stale worker from an earlier standalone probe was killed.
- No venv was created; no package was installed; nothing was committed.

## 9. Remaining WebRTC gaps

- **No native WebRTC.** `/webrtc/*` still depends on the external
  GStreamer/Python helper; `webrtc-rs`/`str0m` is future work.
- **No browser or controller-dashboard run in this report.** The live peer here
  was GStreamer. A later run validated
  `POST /api/v1/streams/{node}/webrtc`, Chromium ICE/DTLS, and dashboard media;
  see
  [`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).
  A further matrix run added Playwright Firefox 159.0 (OpenH264 2.6.0) and
  Chromium 159 iPhone emulation; see
  [`gate2-live-webrtc-matrix-report.md`](gate2-live-webrtc-matrix-report.md).
  Safari/WebKit and a NAT/STUN/TURN/trickle matrix remain open.
- **No trickle ICE.** Candidates travel inside the offer/answer SDP after a
  bounded gather. There is no STUN/TURN and no mDNS obfuscation; host
  candidates only. A slow gather can return a candidate-less answer.
- **No RTCP/statistics assertion.** The bitrate ack proves the encoder
  property was accepted, not that the sender's measured rate changed. No
  `getStats`/RTCP report is collected.
- **Session lifecycle.** A session is removed when its `appsrc` push fails.
  A peer that closes while pushes still succeed can leave the session (and
  encoder) alive; the live run still acknowledged bitrate shortly after the
  client closed. Connection-state-driven cleanup is not implemented.
- **Audio is monitor-only.** Validated with a disposable null-sink monitor and
  a synthetic tone, not guest audio or a real output device. Without a monitor
  source the helper builds a video-only pipeline and an offered audio m-line
  comes back `inactive`.
- **No codec switching in WebRTC.** `/codec` does not reconfigure the WebRTC
  encoder.
- **QMP source cost.** WebRTC still opens its own capture source from the
  shared QMP control client (one `screendump` per frame), so it does not share
  a decoded frame tap with HLS.
