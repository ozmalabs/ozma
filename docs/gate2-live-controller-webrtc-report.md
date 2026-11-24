# Gate 2 controller WebRTC proxy and browser/dashboard report

**Scope:** the Python controller's `/api/v1/streams/{node}/webrtc` proxy, the
dashboard's WebRTC/KVM modal, and the Rust node's helper-backed WebRTC media,
against a disposable headless QEMU display source.

**Result: PASS for the controller proxy and a headless Chromium peer.** A real
GStreamer offer through the controller returned the Rust helper's answer
(9 candidates, `answer`/`type`, DTLS fingerprint), completed ICE/DTLS, and
decoded **153 H.264 frames at 720x400** plus **519 Opus buffers at 48 kHz
stereo**. A default headless Chromium 145 dashboard session then established
`connected`/`ice-connected`, decoded **86 video frames at 720x400 / 15 fps**,
and received **295 audio packets (71,335 bytes)**. The dashboard reported
`WebRTC 16f 16fps 23KB`; there were no page, console, or HTTP errors.

This is **not** a complete Gate 2 pass and **not** a native-WebRTC or browser
matrix pass. It used an OS-less disposable QEMU process, the external
GStreamer/Python helper, host ICE candidates only, and one Chromium build. See
[Remaining gaps](#remaining-gaps).

> No `sudo`, no package install outside a `/tmp` venv, no libvirt domain, and
> no commit were used. Every QEMU, node, helper, controller, browser, null
> sink, port, and temporary file was removed; see [Cleanup](#cleanup).

> **Follow-up (2026-11-24, browser matrix):** Playwright Firefox 159.0
> (OpenH264 2.6.0) and Chromium 159 iPhone emulation later decoded H.264/Opus
> through the same dashboard path. Safari/WebKit, TURN, a distinct `srflx`
> candidate, and trickle ICE remain unvalidated; see
> [`gate2-live-webrtc-matrix-report.md`](gate2-live-webrtc-matrix-report.md).

## 1. Run metadata

| Item | Value |
|---|---|
| Date | 2026-11-24, ~14:42–14:52 AEDT (03:42–03:52Z) |
| Repo | `/home/matt/ozma/ozma` |
| HEAD at start | `436b80cf` (`feat(gate2): fix D-Bus, HLS pacing, WebRTC media, and live validation`) |
| Working tree | this run's controller/worker/script/doc edits are uncommitted; unrelated edits to `ozma-capture/src/dbus_display.rs` and `ozma-node/src/hls.rs` were present and were not touched |
| QEMU | 11.1.3, TCG, 720x400 VGA, no guest |
| Encoder | x264 helper fallback (`x264enc`, no hardware encoder dependency) |
| Node binary | `target/debug/ozma-node`, `ozma-node 0.1.0` |
| Controller | temporary copy of `controller/` run from `/tmp` with an isolated venv |
| Browser | Playwright 1.58.0, Chrome for Testing 145.0.7632.6, headless |
| Ports | controller 42327, node HTTP 52749, node HID/UDP 54840 |
| Temp root | `/tmp/opencode/gate2-ctrl-webrtc` (removed) |
| Audio | disposable null sink `ozma-gate2-ctrl-webrtc-test.monitor`, 440 Hz tone |

## 2. Live source and node

A disposable headless QEMU process exposed a real QMP `screendump` display:

```bash
qemu-system-x86_64 -name ozma-gate2-ctrl-webrtc \
  -machine pc,accel=tcg -m 256 -nodefaults \
  -device VGA -device qemu-xhci,id=xhci -device usb-kbd,bus=xhci.0 \
  -display none \
  -qmp "unix:/tmp/opencode/gate2-ctrl-webrtc/qemu/qmp.sock,server=on,wait=off" \
  -pidfile /tmp/opencode/gate2-ctrl-webrtc/qemu/qemu.pid -daemonize
```

QMP `qmp_capabilities`, `query-status` (`running`), and `screendump`
(`P6 720x400`) all succeeded. A disposable null sink and a 440 Hz tone supplied
audio:

```bash
pactl load-module module-null-sink sink_name=ozma-gate2-ctrl-webrtc-test
gst-launch-1.0 -q audiotestsrc is-live=true wave=sine freq=440 \
  ! audioconvert ! audioresample \
  ! audio/x-raw,format=F32LE,rate=48000,channels=2 \
  ! pulsesink device=ozma-gate2-ctrl-webrtc-test
```

The node ran with QMP input/power, HLS, and the x264 WebRTC helper:

```bash
./target/debug/ozma-node \
  --name ozma-gate2-ctrl-webrtc --profile vm \
  --qmp-socket /tmp/opencode/gate2-ctrl-webrtc/qemu/qmp.sock \
  --input-provider qmp --power-provider qmp \
  --http-port 52749 --hid-udp-port 54840 \
  --cap hid,video,audio,power --machine-class server \
  --register-url http://127.0.0.1:42327 --register-host 127.0.0.1 \
  --heartbeat-interval 5 \
  --hls --hls-dir /tmp/opencode/gate2-ctrl-webrtc/node/hls \
  --hls-fps 5 --hls-bitrate-kbps 1000 --ffmpeg-bin /usr/bin/ffmpeg \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$PWD/softnode/gst_webrtc_worker.py" \
  --webrtc-width 720 --webrtc-height 400 --webrtc-fps 15 \
  --webrtc-audio-source ozma-gate2-ctrl-webrtc-test.monitor \
  --webrtc-audio-backend pulse
```

`GET /api/v1/connection` reported `hls: "running"`, `webrtc: "running"`,
`webrtc_error: null`, and `webrtc_audio_configured: true`. The controller
accepted direct registration and heartbeat; `GET /api/v1/nodes` showed one node
with `api_port: 52749`, `stream_url:
http://127.0.0.1:52749/stream/stream.m3u8`, and capabilities
`hid,video,audio,power`.

## 3. Controller WebRTC proxy — PASS

The dashboard's exact route and body were used:

```bash
POST http://127.0.0.1:42327/api/v1/streams/ozma-gate2-ctrl-webrtc._ozma._udp.local./webrtc
Content-Type: application/json

{"type":"offer","sdp":"v=0..."}
```

The route returned HTTP 200 with a real answer, not a 404/500:

```text
v=0
a=group:BUNDLE video0 audio1
m=video 9 UDP/TLS/RTP/SAVPF 96
a=mid:video0
a=setup:active
a=rtpmap:96 H264/90000
a=sendonly
a=fingerprint:sha-256 5A:EA:...
a=candidate:1 1 UDP ... 192.168.40.19 49980 typ host
... 9 candidates ...
m=audio 9 UDP/TLS/RTP/SAVPF 97
a=mid:audio1
a=setup:active
a=rtpmap:97 OPUS/48000
a=sendonly
```

The offer had 44 lines / 9 candidates; the answer had 39 lines / 9 candidates.
The answer's DTLS fingerprint and host candidates are produced by the Rust
helper's GStreamer session, not synthesized by the controller.

A real GStreamer client was pointed at the controller proxy (not the node) with
`scripts/gate2/webrtc-client.py`:

```bash
python3 scripts/gate2/webrtc-client.py \
  --node-url http://127.0.0.1:42327 \
  --offer-url http://127.0.0.1:42327/api/v1/streams/$NODE_ID/webrtc \
  --bitrate-url http://127.0.0.1:42327/api/v1/nodes/$NODE_ID/proxy/webrtc/bitrate \
  --audio --duration 10 --bitrate 4000000
```

Result:

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
      "buffers": 153,
      "bytes": 66096000,
      "caps": "video/x-raw, format=I420, width=720, height=400, framerate=15/1"
    },
    "audio": {
      "buffers": 519,
      "bytes": 1992960,
      "caps": "audio/x-raw, format=S16LE, rate=48000, channels=2"
    }
  },
  "bitrate": {"ok": true, "body": {"bitrate": 4000000, "ok": true}}
}
```

The bitrate POST went through the controller's
`/api/v1/nodes/{node}/proxy/webrtc/bitrate` route to the node and then to the
helper; the helper acknowledged 4 Mbps.

## 4. Browser/dashboard — PASS with Chromium

The controller's dashboard is `GET /dashboard.html`; the default UI mode is
`simple`. The run set `localStorage['ozma-ui-mode']='full'`, loaded the page,
and activated the node. Evidence from the final Chromium run:

| Check | Result |
|---|---|
| Page | `200`, title `Ozma — Dashboard` |
| Node card | one card for `ozma-gate2-ctrl-webrtc._ozma._udp.local.`, host/hw/role/fw shown |
| Capabilities | `HID VIDEO AUDIO POWER` |
| Power/control buttons | `⏻ On`, `Off`, `Reset`, `Force`, `Cycle`, `Activate`, `▶ Stream`, `🖥 Console`, `▶ Terminal`, `⬡ USB` |
| Snapshot preview | `img[data-snap-url]` completed at 720x400 via `/api/v1/streams/.../snapshot` |
| Activate | card changed to `ACTIVE` |
| Power On | inline status `on OK` |
| Stream modal | opened; `window._kvmPeerConnection.connectionState == "connected"`, ICE `connected`, signaling `stable`, remote description `answer` |
| Video element | `videoWidth=720`, `videoHeight=400`, `readyState=4`, `paused=false`, `srcObject` present |
| Inbound RTP | video `framesDecoded=86`, `framesPerSecond=15`; audio `packetsReceived=295`, `bytesReceived=71335` |
| Dashboard status | `WebRTC 16f 16fps 23KB` |
| Errors | no console messages, page errors, failed requests, or HTTP >=400 responses |

The dashboard uses WebRTC first (`/api/v1/streams/{node}/webrtc`), then HLS
(`/api/v1/streams/{node}/hls/stream.m3u8`), then snapshots. This run's live
modal was WebRTC, not HLS or snapshot-only. A screenshot was captured in the
temporary evidence directory and removed with it.

### Pre-fix browser failure

Before the payload-type fix below, the same Chromium page reached ICE
`connected` but the worker's answer was `a=inactive` for both m-lines, the
`video` element stayed at 0x0, and `getStats()` showed no `inbound-rtp`. The
cause was the worker hard-coding H.264/Opus payload types 96/97. Chrome maps 96
to VP8 and uses 103+ for H.264, so the payloader and answer did not match.
After using the offered payload types, the answer became `a=sendonly` with
`a=rtpmap:103 H264/90000` and `a=rtpmap:111 OPUS/48000/2`, and the same
dashboard path decoded frames. This failure is recorded here rather than
presented as a pass.

## 5. `/codec` through the controller — route fixed, not proxied

The controller previously registered its catch-all
`GET /api/v1/streams/{node_id:path}` before the `GET /codec` and `GET /stats`
routes. Because `{node_id:path}` matches slashes, those specific GET routes
were unreachable: the request matched `get_stream` with a node id ending in
`/codec` and returned `404 {"detail":"No stream for this node"}`. The catch-all
was moved after the specific routes. (`POST /codec` was not shadowed — the
catch-all is GET-only — but it returns 404 for an `hls-remote` stream because
`switch_codec` does not support that stream type.)

After the route-order fix (before the `/codec` proxy in §5.1):

| Request | Result |
|---|---|
| `GET /api/v1/streams/{id}/codec` | `200`; controller-local encoder list, empty `stats`/`config`, `current_encoder: ""` |
| `POST /api/v1/streams/{id}/codec` | `404 {"detail":"Node not found or codec switching not supported for this stream type"}` |
| `GET /api/v1/streams/{id}/stats` | `200 {"active": false}` |
| `GET /api/v1/streams/{id}` | `200`, type `hls-remote`, node URL |
| direct node `GET /codec` | `200`, `current_encoder: "h264"`, `available: ["h264"]` |
| direct node `POST /codec` | `200`, `applied: true`, selected `h264_vaapi`, HLS restarted and stayed `running` |

Conclusion at the time of this run: the controller `/codec` route **did not
reach the node** for an `hls-remote` stream. It reported controller-side
encoders and stats, and POST was a typed 404. Direct node `/codec` works (it
restarted HLS to VAAPI, and the controller-proxied HLS segment still probed as
H.264 720x400). The proxy gap was closed after this run; see §5.1.

### 5.1 Follow-up: controller `/codec` proxy

The controller now proxies codec reads and switches for `hls-remote` streams to
the node's `/codec` route when a node API endpoint is known:

- `GET /api/v1/streams/{id}/codec` returns the node's JSON (`current_encoder`,
  `available`, `override`, `receipt`, `pending_restart`, `applied`) instead of
  controller-local probe data.
- `POST /api/v1/streams/{id}/codec` forwards `CodecConfig.to_dict()` as JSON to
  the node and returns the node's result. The node's typed 400/502/503 bodies
  and status are preserved, so a rejected override is no longer a controller
  404.
- A node that cannot be reached or times out produces a controller `502` with a
  `Node unreachable`/`timed out` detail, never a success-shaped local response.
- Controller auth/scope checks still run before the proxy (`read` for GET,
  `write` for POST). Non-remote captures and node ids without a known API
  endpoint keep the previous controller-local behavior.

The endpoint is taken from the registered node's `api_port` (falling back to
`stream_port`, then the authority of the remote HLS URL). A fake-node HTTP
suite asserts GET/POST forwarding, request headers/body, 400/502 passthrough,
unreachable, timeout, auth, and local fallback:
`tests/unit/controller/test_stream_codec_proxy.py` (10 tests, including the
parametrised 400/502 error cases). A later live run repeated the route through
the controller against a real Rust node
([`gate2-final-e2e-report.md`](gate2-final-e2e-report.md) §5.7): POST
`{"codec":"h264","hw_accel":"software","bitrate":"1500k"}` returned
`applied:true`, `pending_restart:false`, `encoder:"libx264"`, restarted HLS,
and the next segment still probed as H.264.

Still separate: the controller's codec probe/encoder database and stats remain
controller-local and are not merged with the node's `EncoderRegistry`. The
controller also does not send a node bearer token, so a node started with
`--api-token` will reject the proxied call (the node's status/body is passed
through).

## 6. Fixes made

1. **Controller route shadowing (`controller/api.py`).** Moved the catch-all
   `GET /api/v1/streams/{node_id:path}` below `/codec`, `/stats`, and
   `/adaptive` so those routes are reachable. This is a routing bug fix; no
   behavior was redesigned.

2. **Worker payload types (`softnode/gst_webrtc_worker.py`).** Added
   `offered_payload_type()` and passed the peer's offered H.264/Opus payload
   types into `build_pipeline_string()` / `_build_pipeline()` instead of
   hard-coding 96/97. The GStreamer client still negotiates 96/97; Chrome now
   gets 103/111. Without this, the browser answer was `a=inactive` and no
   media was decoded.

3. **Client proxy targeting (`scripts/gate2/webrtc-client.py`).** Added
   `--offer-url` and `--bitrate-url` overrides so the same real GStreamer peer
   can exercise the controller's `/api/v1/streams/{node}/webrtc` and
   `/api/v1/nodes/{node}/proxy/webrtc/bitrate` routes. Defaults are unchanged.

4. **Tests.** `softnode/gst_webrtc_worker_test.py` now covers Chrome and
   GStreamer payload-type selection and asserts the generated pipeline uses the
   offered PTs. 26 tests pass; `py_compile` passes for the worker, client, and
   controller.

## 7. Tests run

```text
python3 softnode/gst_webrtc_worker_test.py
  -> 26 passed

python3 -m py_compile softnode/gst_webrtc_worker.py \
  scripts/gate2/webrtc-client.py controller/api.py
  -> OK

scripts/gate2/webrtc-client.py (controller offer URL)
  -> connected / ICE completed / 153 video + 519 audio buffers / bitrate ack

Playwright headless Chromium 145 dashboard run
  -> WebRTC connected / 86 video frames / 15 fps / 295 audio packets

python3 -m pytest tests/unit/controller/test_stream_codec_proxy.py
  -> 10 passed

python3 -m py_compile controller/api.py controller/stream.py \
  tests/unit/controller/test_stream_codec_proxy.py
  -> OK
```

The Rust node unit/parity suite was not rerun because no Rust source used by
this path was changed. Pre-existing unrelated edits in
`ozma-capture/src/dbus_display.rs` and `ozma-node/src/hls.rs` were not built or
claimed here.

## 8. Cleanup

- The node received `SIGINT`; it reaped its HLS ffmpeg and WebRTC helper.
- QEMU was terminated with its pidfile; the QMP socket went with the temp root.
- The tone `gst-launch` process was killed.
- The null sink was unloaded with `pactl unload-module 536870916`.
- The controller received `SIGINT` and exited. Its temporary capture ffmpeg
  child was terminated during cleanup.
- The `/tmp` venv, controller copy, browser processes, evidence, and temp root
  were removed.
- No package was installed outside the temporary venv; no commit was made.

## Remaining gaps

- **No STUN/TURN and no trickle ICE.** Candidates are host candidates embedded
  in the offer/answer after a bounded gather. This run is same-host/LAN only; a
  browser behind NAT or a different network is untested and would likely fail.
  Chrome's default mDNS `.local` candidates did not prevent connection here
  because the helper's host candidate was directly reachable.
- **Browser matrix.** This run used Chromium 145 headless. A follow-up added
  Playwright Firefox 159.0 with OpenH264 2.6.0 and Chromium 159 iPhone
  emulation ([`gate2-live-webrtc-matrix-report.md`](gate2-live-webrtc-matrix-report.md)).
  Safari/WebKit, hardware decode, multiple concurrent peers, and
  reconnect/session teardown remain untested.
- **External helper only.** There is no native `webrtc-rs`/`str0m` path. The
  node still depends on `softnode/gst_webrtc_worker.py` and GStreamer.
- **Controller `/codec` proxying.** The controller now proxies GET/POST to the
  node for `hls-remote` streams (§5.1), but this has only fake-node test
  coverage so far; it was not repeated against a live Rust node. Controller
  codec probe/stats data and the node's `EncoderRegistry` remain separate, and
  the proxy does not send a node bearer token.
- **Audio is monitor-only.** A disposable null-sink monitor and synthetic tone
  were used. Guest audio, a real output device, and audible decode quality were
  not validated.
- **No RTCP/rate assertion.** `/webrtc/bitrate` proves the encoder property was
  accepted. Browser `getStats()` proves frames/bytes arrived, but no RTCP
  sender report or measured encoder-rate assertion was collected.
- **Session lifecycle.** A peer that closes while the worker can still push
  frames may leave a session/encoder alive; connection-state-driven cleanup is
  not implemented.
- **No guest OS.** QEMU had no disk or guest, so guest-visible input, ACPI, and
  guest audio/video content remain outside this report. Gate 2 as a whole still
  needs a real guest scenario.
- **Controller proxy is loopback-only.** It still connects to
  `127.0.0.1:<api_port>` and does not send a node bearer token.
