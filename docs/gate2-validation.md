# Gate 2 live validation

Gate 2 from [`rust-reconciliation-plan.md`](rust-reconciliation-plan.md) §3:

> the unmodified Python controller can discover, activate, input, stream HLS,
> and negotiate WebRTC with a Rust VM node while **no Python soft node process
> is running**. One real QEMU VM with QMP passes the demo scenario end to end.

This document describes the **opt-in, manual** validation harness for that
gate. It validates a setup that is already running. It does **not** configure,
start, stop, define, or edit a VM; it does not start the controller, the node,
ffmpeg, or the WebRTC helper; and it never uses `sudo` or a destructive VM
operation. Provisioning remains separate work (`ozma-softnode --provision` is
opt-in and is not run here).

> **Do not claim Gate 2 from this document alone.** Each row needs a captured
> artifact/log from an actual run, attached to the parity matrix row. A harness
> `PASS` covers only the checks it performs; guest-visible input, browser media,
> the controller dashboard, and the controller regression suite remain manual.

## 1. Prerequisites

| Component | Required state | How to check |
|---|---|---|
| QEMU VM | A running VM with a QMP control socket and a display. A libvirt domain is preferred, but a direct-QEMU process is acceptable with `OZMA_GATE2_SKIP_LIBVIRT=1`. | `virsh list --all`, `virsh domstate <domain>`, or the QEMU process's `-qmp` argument. |
| QMP socket | Unix socket that accepts a connection and sends the QMP greeting. QEMU example: `-qmp unix:/run/ozma/ozma-gate2.qmp,server=on,wait=off`. The user running `ozma-node` must be able to connect. | `scripts/gate2/qemu-preflight.sh` performs a read-only connect and greeting check. |
| VNC (optional) | A VNC endpoint only if you want the ffmpeg VNC capture path instead of QMP screendump. QEMU example: `-vnc 127.0.0.1:1` (TCP port 5901). | `OZMA_VNC_HOST=127.0.0.1 OZMA_VNC_PORT=5901 scripts/gate2/qemu-preflight.sh`. |
| ffmpeg | HLS needs `libx264` and the `hls` muxer. VNC capture additionally needs the VNC demuxer. Some distro builds report `Unknown format 'vnc'`. | `ffmpeg -hide_banner -demuxers \| grep vnc`; `ffmpeg -hide_banner -encoders \| grep libx264`; `ffmpeg -hide_banner -muxers \| grep hls`. |
| WebRTC helper | The existing `softnode/gst_webrtc_worker.py`, Python 3 with `gi`/GStreamer, and `webrtcbin`, `rtph264pay`, plus `x264enc` or `nvh264enc`. | `python3 -c 'import gi; gi.require_version("GstWebRTC","1.0")'`; `gst-inspect-1.0 webrtcbin`; `gst-inspect-1.0 x264enc`. |
| Python controller | The **unmodified** controller from `controller/`, listening on its normal port (7380). Its own Python requirements must be installed. | `python3 controller/main.py --host 127.0.0.1 --port 7380`; `curl -sS http://127.0.0.1:7380/health`. |
| Rust node | `ozma-node` built from this tree. | `cargo build -p ozma-node` (binary at `target/debug/ozma-node`). |
| No Python soft node | No `soft_node.py`, `virtual_node.py`, `desktop_node.py`, or Python soft-node process may be serving the VM. | `pgrep -af 'soft_node|virtual_node|desktop_node'` should show nothing relevant. |
| Ports free | The node's HTTP/HID ports (defaults 7382/7331) must not be owned by a Python soft node. | `ss -lntup \| grep -E ':(7331|7382)\b'`. |

The node and the controller should run on the same host (or share a network
namespace). The unmodified controller's input, HLS, WebRTC, and snapshot
proxies connect to `127.0.0.1:<api_port>`, not the registered `host` (see
§7.6). A remote node can serve direct URLs, but the controller dashboard proxy
will fail.

## 2. Start the controller and node

Use three terminals. Start the controller first so the node's one-shot direct
registration succeeds.

### Terminal 1: unmodified controller

```bash
cd controller
python3 main.py --host 127.0.0.1 --port 7380
```

The API port defaults to 7380. Controller auth is disabled by default; if you
enabled it, set `OZMA_CONTROLLER_TOKEN` for the harness and use the
controller's normal login/token flow in the browser. The harness does not
modify controller configuration.

### Terminal 2: Rust VM node

```bash
NODE_NAME=ozma-vm-gate2
NODE_ID="${NODE_NAME}._ozma._udp.local."
NODE=http://127.0.0.1:7382
CTRL=http://127.0.0.1:7380
QMP_SOCKET=/run/ozma/ozma-gate2.qmp
HLS_DIR=/tmp/ozma-gate2-hls

./target/debug/ozma-node \
  --name "$NODE_NAME" \
  --profile vm \
  --qmp-socket "$QMP_SOCKET" \
  --input-provider qmp \
  --power-provider qmp \
  --http-port 7382 \
  --hid-udp-port 7331 \
  --cap hid,video,audio \
  --machine-class server \
  --hls \
  --hls-dir "$HLS_DIR" \
  --hls-fps 15 \
  --hls-bitrate-kbps 4000 \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$PWD/softnode/gst_webrtc_worker.py" \
  --webrtc-width 1280 \
  --webrtc-height 720 \
  --webrtc-fps 15 \
  --register-url "$CTRL"
```

Notes:

- `--input-provider qmp` makes the guest input path deterministic. The VM
  profile's default chain is `uinput -> qmp -> hid-gadget -> recording`; if the
  user can open `/dev/uinput`, that sink wins instead of QMP.
- With `--qmp-socket`, the node creates one shared QMP control client and
  serializes power, QMP capture, snapshots, and QMP input through it. Add
  `--qmp-input-socket` only when QEMU has a second `-qmp` chardev: input then
  uses that dedicated input socket and never opens another control client.
- `--power-provider qmp` is the default when `--qmp-socket` is set; the explicit
  flag documents the intent. Use `--libvirt-domain <domain>` (and omit the
  power override) if power must go through libvirt instead.
- Do **not** set `--api-token` / `OZMA_API_TOKEN` for controller-proxied
  validation: the unmodified controller proxies do not send a node bearer
  token. Direct node checks may use a token.
- `--register-url` is a one-shot POST at startup. If the controller was not up,
  restart the node; there is no registration retry loop.
- `--register-host` is optional. Set it only when the controller/browser should
  use a specific address for the node's direct `stream_url`; the controller's
  own proxy still uses `127.0.0.1`.

Equivalent environment variables (the CLI flags override them where both
exist):

```bash
export OZMA_PROFILE=vm
export OZMA_QMP_SOCKET=/run/ozma/ozma-gate2.qmp
export OZMA_QMP_INPUT_SOCKET=/run/ozma/ozma-gate2-input.qmp  # optional
export OZMA_LIBVIRT_DOMAIN=ozma-gate2                        # optional power
export OZMA_HLS_DIR=/tmp/ozma-gate2-hls
export OZMA_HLS_FPS=15
export OZMA_HLS_BITRATE_KBPS=4000
export OZMA_FFMPEG_BIN=/usr/bin/ffmpeg
export OZMA_WEBRTC_HELPER=/usr/bin/python3
export OZMA_WEBRTC_HELPER_SCRIPT="$PWD/softnode/gst_webrtc_worker.py"
export OZMA_WEBRTC_WIDTH=1280
export OZMA_WEBRTC_HEIGHT=720
export OZMA_WEBRTC_FPS=15
export OZMA_API_TOKEN=...                                    # direct-only use
```

There is no environment variable for `--hls` or `--register-url`; pass those
flags.

### VNC capture alternative

Keep the QMP socket for power and input, but add a VNC endpoint and do **not**
set `--capture-provider qmp`:

```bash
./target/debug/ozma-node \
  --name "$NODE_NAME" \
  --profile vm \
  --qmp-socket "$QMP_SOCKET" \
  --input-provider qmp \
  --power-provider qmp \
  --vnc-host 127.0.0.1 \
  --vnc-port 5901 \
  --vnc-width 1280 \
  --vnc-height 720 \
  --vnc-fps 15 \
  --http-port 7382 \
  --hid-udp-port 7331 \
  --cap hid,video,audio \
  --machine-class server \
  --hls \
  --hls-dir "$HLS_DIR" \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$PWD/softnode/gst_webrtc_worker.py" \
  --register-url "$CTRL"
```

The VNC flags select VNC capture when no explicit capture provider is given.
An explicit `--capture-provider qmp` together with the VNC flags is a
configuration error. The VNC path requires an ffmpeg build with the VNC
demuxer; otherwise use QMP screendump capture.

## 3. Run the harness

Set the same identifiers and run the orchestration wrapper:

```bash
export OZMA_NODE_URL=http://127.0.0.1:7382
export OZMA_NODE_ID=ozma-vm-gate2._ozma._udp.local.
export OZMA_CONTROLLER_URL=http://127.0.0.1:7380

export OZMA_QEMU_DOMAIN=ozma-gate2
export OZMA_QMP_SOCKET=/run/ozma/ozma-gate2.qmp
export OZMA_HLS_DIR=/tmp/ozma-gate2-hls
export OZMA_WEBRTC_HELPER=/usr/bin/python3
export OZMA_WEBRTC_HELPER_SCRIPT="$PWD/softnode/gst_webrtc_worker.py"
# Optional VNC:
# export OZMA_VNC_HOST=127.0.0.1
# export OZMA_VNC_PORT=5901

scripts/gate2/run-gate2.sh
```

The wrapper runs `qemu-preflight.sh` and `check-node.sh` and writes a
timestamped report to `${OZMA_GATE2_REPORT_DIR:-/tmp/ozma-gate2}/gate2-*.log`.
It prints the report on stdout, records both exit codes, and exits non-zero if
either stage failed. It never starts a VM or the controller.

Run the pieces separately when debugging:

```bash
scripts/gate2/qemu-preflight.sh
scripts/gate2/check-node.sh
```

For the Rust opt-in test (it does not start anything either):

```bash
OZMA_GATE2_NODE_URL=http://127.0.0.1:7382 \
OZMA_GATE2_NODE_ID=ozma-vm-gate2._ozma._udp.local. \
cargo test -p ozma-node --test gate2_live -- --ignored --nocapture
```

The test is `#[ignore]`d and env-gated: without
`OZMA_GATE2_NODE_URL` it prints a skip and passes. Set
`OZMA_GATE2_WEBRTC_SDP_FILE=/path/to/offer.json` to exercise a real browser
offer. Set `OZMA_GATE2_HLS_ADVANCE_SECS=0` to skip the playlist-advance
comparison. `check-node.sh` accepts the same
`OZMA_GATE2_WEBRTC_SDP_FILE` variable.

## 4. Step-by-step checklist

Use `$NODE` for the node base URL and `$NODE_ID` for the full controller id
from §2. Quote the id; it ends with a dot. Save the command output and the
requested artifact for each row.

### Step 1 — Preflight

```bash
scripts/gate2/qemu-preflight.sh
```

Expected: `PASS` for the libvirt domain (or an explicit skip), QMP greeting,
ffmpeg `libx264`/HLS, writable HLS output parent, helper script/GStreamer, and
the optional controller health check. Fix failures with the printed
remediation; do not use this harness to start or edit the VM.

Artifact: the preflight output or the `run-gate2.sh` report.

### Step 2 — Discovery and registration

```bash
curl -sS "$CTRL/api/v1/nodes" | python3 -m json.tool
curl -sS -X POST "$CTRL/api/v1/nodes/$NODE_ID/activate"
```

Expected in the node object:

- `id == "$NODE_ID"`
- `port == 7331` (the HID UDP port)
- `api_port == 7382` (an integer after the controller parses it)
- `stream_url == "http://<registered host>:7382/stream/stream.m3u8"` (the
  controller keeps `stream_port`/`stream_path` internally but exposes the
  composed URL in `to_dict`)
- `capabilities` includes `hid`, `video`, and `audio` (if your `--cap` does)
- `machine_class == "server"` when started with `--machine-class server`
- activation returns `{"ok":true,"active_node_id":"..."}`

Wait at least two heartbeat intervals (default 30 s each) and re-fetch the
node list. The node should stay present. If registration failed at startup,
restart the node after the controller is healthy.

Artifact: the JSON node object, the activation response, and the node log lines
for registration/heartbeat.

### Step 3 — Node `/api/v1/connection`

```bash
curl -sS "$NODE/api/v1/connection" | python3 -m json.tool
```

Expected keys and values for the QMP+HLS+helper setup:

```json
{
  "ok": true,
  "node_id": "ozma-vm-gate2._ozma._udp.local.",
  "profile": "vm",
  "api_port": 7382,
  "quic_port": 7384,
  "hid_udp_port": 7331,
  "sink_kind": "qmp",
  "power": {"status": "running", "available": true},
  "input": {"sink_kind": "qmp", "available": true, "injecting": true},
  "hls": "running",
  "hls_error": null,
  "webrtc": "running",
  "webrtc_error": null,
  "audio": "not-implemented"
}
```

`hls` is `not-configured` without `--hls`, `failed` plus `hls_error` when a
configured pipeline cannot start, and `running` only after `stream.m3u8`
exists. `webrtc` is `not-configured` without a helper, `failed` plus
`webrtc_error` when the helper cannot start, and `running` when the helper's
signaling socket is connected. `audio` is honestly `not-implemented`.

Artifact: the full connection JSON.

### Step 4 — Power state

```bash
curl -sS "$NODE/power/state"
curl -sS "$CTRL/api/v1/nodes/$NODE_ID/power/state"
```

Expected node response: `{"status":"running","available":true}` (or the VM's
actual QMP/libvirt state). Expected controller response: the same through the
proxy. `available:false` means no power backend is configured and Gate 2 power
parity is not met.

Do **not** call `/power/off`, `/power/stop`, or `/power/reset` on a VM you care
about. The harness never does. If you test an action manually, use a disposable
VM and note that QMP `stop` is a pause, not a destroy; force-off falls back to
ACPI powerdown.

Artifact: both responses.

### Step 5 — Input key and mouse

Direct node routes:

```bash
curl -sS -X POST "$NODE/input/key" \
  -H 'Content-Type: application/json' \
  -d '{"keycode":30,"down":true}'
curl -sS -X POST "$NODE/input/key" \
  -H 'Content-Type: application/json' \
  -d '{"keycode":30,"down":false}'

curl -sS -X POST "$NODE/input/mouse" \
  -H 'Content-Type: application/json' \
  -d '{"x":16000,"y":16000,"action":"move"}'
curl -sS -X POST "$NODE/input/mouse" \
  -H 'Content-Type: application/json' \
  -d '{"x":16000,"y":16000,"action":"click","button":1}'
```

`keycode` is a Linux evdev keycode; `30` is `KEY_A`. Expected key response:
`{"ok":true,"sink":"qmp"}` (or `"uinput"` if that sink won). A recording-only
sink also returns `"injected":false`; that is not Gate 2 input parity. Expected
mouse response: `{"ok":true}`. Button `1` is left, `2` right, `4` middle.

Controller proxy equivalents:

```bash
curl -sS -X POST "$CTRL/api/v1/nodes/$NODE_ID/proxy/input/key" \
  -H 'Content-Type: application/json' \
  -d '{"keycode":30,"down":true}'
curl -sS -X POST "$CTRL/api/v1/nodes/$NODE_ID/proxy/input/mouse" \
  -H 'Content-Type: application/json' \
  -d '{"x":16000,"y":16000,"action":"move"}'
```

Expected: the same `ok` responses. The controller proxy connects to
`127.0.0.1:<api_port>` and does not send `OZMA_API_TOKEN`.

Artifact: command output **and** a guest screenshot/video showing the typed
character and pointer movement/click. The HTTP response alone does not prove
guest injection.

### Step 6 — HLS playlist advance

```bash
curl -sS "$NODE/stream/stream.m3u8" | tee /tmp/gate2-playlist-1.m3u8
sleep 5
curl -sS "$NODE/stream/stream.m3u8" | tee /tmp/gate2-playlist-2.m3u8
```

Expected: both responses start with `#EXTM3U`; the media sequence or segment
list changes between fetches. The harness polls for an advance for up to 10 s by
default because QMP screendump capture can be slower than the nominal HLS
frame rate. `/stream/*` is intentionally mounted outside the bearer guard so
the controller can proxy it without credentials.

Controller proxy:

```bash
curl -sS "$CTRL/api/v1/streams/$NODE_ID/hls/stream.m3u8" \
  | tee /tmp/gate2-controller-playlist.m3u8
```

Expected: HTTP 200 and a playlist. The controller rewrites the URL to
`http://127.0.0.1:<api_port>/stream/<filename>`; a 503 here usually means the
node API port is not local to the controller.

Playable artifact:

```bash
SEGMENT=$(grep -v '^#' /tmp/gate2-playlist-2.m3u8 | head -n 1)
curl -sS "$NODE/stream/$SEGMENT" -o /tmp/gate2-segment.ts
ffprobe -v error -show_entries stream=codec_name,width,height \
  -of default=nw=1 /tmp/gate2-segment.ts
```

Expected: `codec_name=h264` and non-zero dimensions. A browser or `ffplay`
should play the controller's HLS URL.

Artifact: both playlists, the controller playlist, `ffprobe` output, and a
player screenshot if possible.

### Step 7 — WebRTC offer

The helper is configured and `connection.webrtc` is `running`. A real browser
offer is required for a meaningful dashboard negotiation; a synthetic SDP only
tests the route. For a browserless real negotiation, use
`scripts/gate2/webrtc-client.py` (below); its direct run is recorded in
[`gate2-live-webrtc-report.md`](gate2-live-webrtc-report.md), and its controller
proxy plus headless Chromium dashboard run is recorded in
[`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).

1. In the browser console (or the dashboard), create a receive-only offer and
   wait for ICE gathering:

```js
const pc = new RTCPeerConnection();
pc.addTransceiver("video", {direction: "recvonly"});
const offer = await pc.createOffer();
await pc.setLocalDescription(offer);
await new Promise(resolve => {
  if (pc.iceGatheringState === "complete") return resolve();
  pc.onicegatheringstatechange = () => {
    if (pc.iceGatheringState === "complete") resolve();
  };
  setTimeout(resolve, 3000);
});
copy(JSON.stringify({sdp: pc.localDescription.sdp, type: pc.localDescription.type}));
```

Paste the clipboard into `/tmp/offer.json`. Then:

```bash
curl -sS -X POST "$NODE/webrtc/offer" \
  -H 'Content-Type: application/json' \
  --data-binary @/tmp/offer.json \
  | tee /tmp/gate2-answer.json
```

Expected: HTTP 200, `{"sdp":"v=0...","type":"answer"}`, non-empty SDP. The
browser should receive H.264 video; keep it in the same page/client that
created the offer so ICE/DTLS can complete.

Controller route:

```bash
curl -sS -X POST "$CTRL/api/v1/streams/$NODE_ID/webrtc" \
  -H 'Content-Type: application/json' \
  --data-binary @/tmp/offer.json \
  | tee /tmp/gate2-controller-answer.json
```

Expected: the same answer shape. The controller proxies to the node's
`/webrtc/offer`. Live evidence: a real GStreamer offer through this route
returned 9 host candidates and a DTLS fingerprint, then completed ICE/DTLS and
decoded video/audio; see
[`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).

Bitrate behavior:

```bash
curl -sS -i -X POST "$NODE/webrtc/bitrate" \
  -H 'Content-Type: application/json' \
  -d '{"bitrate":4000000}'
```

Expected with the checked-in worker: HTTP 200 and
`{"ok":true,"bitrate":4000000}`. The route clamps to 500 kbps–50 Mbps, sends
`{"type":"bitrate","bitrate":<bps>}` to the helper, and returns success only
after the worker's acknowledgement. A helper that ignores the message or
replies `{"ok":false,...}` is a typed 502. Live evidence is in
[`gate2-live-webrtc-report.md`](gate2-live-webrtc-report.md).

Automated GStreamer alternative (no browser automation required):

```bash
python3 scripts/gate2/webrtc-client.py \
  --node-url "$NODE" \
  --audio \
  --duration 8 \
  --bitrate 4000000 \
  --report-json /tmp/gate2-webrtc.json
```

This creates a real receive-only offer, completes ICE/DTLS, counts decoded
H.264 (and Opus when `--audio` is configured) buffers, and prints/exits
nonzero when media does not arrive. To target the controller, add
`--offer-url "$CTRL/api/v1/streams/$NODE_ID/webrtc"` and
`--bitrate-url "$CTRL/api/v1/nodes/$NODE_ID/proxy/webrtc/bitrate"`. A browser
run is still required to prove the controller dashboard path; the headless
Chromium 145 run is in
[`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).

Artifact: the offer, the node answer, the controller answer, a browser
screenshot/video with media, and optionally a `chrome://webrtc-internals` dump.
For the GStreamer alternative, attach the offer/answer SDP and the report JSON.

### Step 8 — Controller dashboard proxy

Open `http://127.0.0.1:7380` in a browser:

1. The Rust node appears in the node list with its capabilities and stream URL.
2. Activating it succeeds.
3. The dashboard's live view/HLS path renders.
4. Dashboard input reaches the guest (key and pointer).
5. The WebRTC path returns an answer and renders media. The live Chromium run
   reported `connected`, 720x400 video, and audio packets; see
   [`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).

This is the row that proves the **unmodified controller** path. The harness's
controller checks cover health, registration, node detail, power proxy, HLS
proxy, and optionally an input-route release; they cannot see the rendered UI.

Artifact: dashboard screenshot(s) showing the node, live view, and input; plus
the harness report.

### Step 9 — Input round trip

With a guest text field focused, send `KEY_A` down/up from the dashboard or the
curl commands in Step 5. The guest must show `a`. Move and click the pointer;
the guest cursor must move and click. Confirm `/api/v1/connection.input` says
`available:true` and `injecting:true`, not `fake-recording`.

Artifact: guest screenshot/video and the connection `input` object.

### Step 10 — Cleanup

Stop the node with `SIGINT`/`SIGTERM` (Ctrl-C). It should reap its HLS ffmpeg
and WebRTC helper and exit. Then:

```bash
pgrep -af 'ozma-node|gst_webrtc_worker|ffmpeg.*ozma' || true
ls -la /tmp/ozma-gate2-hls
```

Remove the HLS directory and stale WebRTC socket/shm files only if you created
them. Do **not** delete the QMP socket; QEMU owns it. Do **not** `virsh
destroy` the VM; leave it in the state you found it. Stop the controller with
Ctrl-C.

Artifact: the process list after shutdown and the node's shutdown log.

## 5. Expected results and pass criteria

| Gate 2 row | Harness check | Expected | Manual artifact still required |
|---|---|---|---|
| Discovery/registration | `check-node.sh` controller node list/detail; heartbeat window | Node present with `api_port`, `stream_url`, `capabilities`, `machine_class`; stays present after ≥2 heartbeat intervals | Node log + controller node JSON |
| `/api/v1/connection` | `check-node.sh` | 200, `ok:true`, `profile:"vm"`, `node_id` matches, `hls`/`webrtc` honest, `input.injecting:true` | Full JSON |
| Power | `check-node.sh` `/power/state` and controller proxy | 200, `available:true`, real status | Both responses |
| Input key/mouse | Manual + optional controller release route | `ok:true` with `sink:"qmp"`/`"uinput"`, guest visible | Guest screenshot/video |
| HLS | `check-node.sh` playlist fetch/advance; controller proxy | 200, `#EXTM3U`, playlist advances, proxy 200, H.264 segment | Two playlists, `ffprobe`, player screenshot |
| WebRTC | `check-node.sh` real-SDP offer (if provided) + `webrtc-client.py` + manual browser | 200 answer with candidates; `webrtc-client.py` reaches ICE/DTLS connected and decodes H.264 (and optional Opus); browser media remains manual | Offer/answers, client report JSON, browser screenshot/internals |
| Controller dashboard | Manual | Node visible, activate, live view, input, WebRTC | Dashboard screenshots |
| Cleanup | Manual + `check-node.sh` not applicable | Node exits, child processes reaped, VM untouched | Process list/log |

A `WARN` or `SKIP` in the harness is an explicit gap, not a pass. In
particular, a minimal-SDP warning means the route exists but no real WebRTC
negotiation was proven; a skipped HLS check means the node was not started with
`--hls`.

## 6. Harness reference

| Script | What it checks | Exit behavior |
|---|---|---|
| `scripts/gate2/qemu-preflight.sh` | `virsh`/domain state, QMP socket + greeting, optional QMP input socket, ffmpeg run/VNC demuxer/`libx264`/HLS muxer, HLS output parent, VNC TCP reachability, WebRTC helper script/Python-GStreamer/elements, optional controller health. | `1` on any failed required check; `0` with warnings/skips; `2` on usage errors. |
| `scripts/gate2/check-node.sh` | `/health`, `/api/v1/connection`, `/power/state`, HLS playlist + advance when running, `/webrtc/offer` only when a helper is configured, and optional controller health/registration/power/HLS/input-route checks. | `1` on any failed check; `0` with warnings/skips; `2` on usage errors. |
| `scripts/gate2/run-gate2.sh` | Runs both and writes `$OZMA_GATE2_REPORT_DIR/gate2-<UTC>.log`. Tokens are reported only as configured/not-configured. | `1` if either stage failed. |
| `ozma-node/tests/gate2_live.rs` | Rust HTTP checks for health/connection/power/HLS and an optional real-SDP WebRTC offer. | `#[ignore]` + env-gated; skips cleanly without `OZMA_GATE2_NODE_URL`. |
| `scripts/gate2/webrtc-client.py` | Real GStreamer `webrtcbin` client: receive-only H.264 (+ optional Opus) offer, ICE/DTLS, decoded-media counters, optional `/webrtc/bitrate`. Uses system Python/GStreamer; installs nothing. | `0` when connected and at least one video buffer arrived; `1` on negotiation/media failure; `2` on usage/environment failure. |

See [`scripts/gate2/README.md`](../scripts/gate2/README.md) for the full
environment-variable table.

## 7. Known honest limits

These are current limits, not harness bugs. They must remain visible in the
parity matrix until the underlying work lands.

1. **QMP screendump latency and scope.** Every frame is a QMP `screendump`
   round trip plus a PPM write/read; throughput is bounded by the polling loop
   (default max 30 fps, usually lower). It captures QEMU's default console only
   (no `device` argument), and the D-Bus framebuffer provider is still not
   implemented. HLS timestamps use the configured `--hls-fps`, not wall-clock
   capture timing, so a very low screendump rate can make playback run fast.
2. **VNC needs a VNC-enabled ffmpeg.** `VncDisplayProvider` spawns
   `ffmpeg -f vnc ...`; some distro ffmpeg builds report
   `Unknown format 'vnc'`. It supports unauthenticated loopback VNC only, scales
   every frame to the configured size, and carries no VNC audio or input. It is
   a transition path, not the destination.
3. **WebRTC is helper-first.** There is no native `webrtc-rs`/`str0m` stack.
   `--webrtc-helper` is required; without it the node reports `not-configured`
   and answers 503. A configured helper that fails is `failed`, never
   `running`. `/webrtc/bitrate` returns success only on the worker's
   `{"ok":true,...}` acknowledgement (typed 502 otherwise) and clamps to
   500 kbps–50 Mbps. ICE is non-trickle: the worker accepts candidates embedded
   in the offer and appends its gathered host candidates to the answer; there
   is no STUN/TURN. A live GStreamer peer (direct and controller proxy) and a
   default headless Chromium dashboard completed ICE/DTLS and decoded H.264 and
   Opus in [`gate2-live-webrtc-report.md`](gate2-live-webrtc-report.md) and
   [`gate2-live-controller-webrtc-report.md`](gate2-live-controller-webrtc-report.md).
   The browser run is one Chromium build on the same host; Firefox/Safari,
   NAT/TURN, multiple peers, and reconnect are untested. `running` by itself
   means the helper's Unix socket is connected, not that a peer completed
   ICE/DTLS.
4. **HLS does not delete old segments.** The ffmpeg argv has no
   `-hls_flags delete_segments`; retained `.ts` files accumulate. The playlist
   keeps four entries by default. `/stream/*` is unauthenticated by design so
   the controller can proxy it; control endpoints remain guarded when a token
   is configured.
5. **QMP has one control client per `-qmp` chardev.** QEMU accepts one client
   per socket. With `--qmp-socket` the node creates one `SharedQmpClient` in
   `main`; power, QMP `screendump` capture, snapshots, and QMP input when no
   dedicated input socket is configured all serialize through that connection
   under one async command mutex. HLS and snapshots each still open a capture
   *source* (and HLS/WebRTC still do not share a frame tap), but a QMP source
   reuses the shared control connection instead of dialing a second client.
   `--qmp-input-socket` selects a dedicated input-only client for input, so the
   control socket remains single-owner. D-Bus display capture still performs
   its own QMP `getfd`/`add_client` handoff and is **not** multiplexed through
   the shared client; do not combine D-Bus capture with concurrent QMP control
   roles on a one-socket VM. A non-QMP source that cannot be opened twice (for
   example a single-client VNC server) can still make one path report `failed`.
6. **The controller proxies to `127.0.0.1`.** `controller/api.py` input, HLS,
   WebRTC, and snapshot proxies use `http://127.0.0.1:<api_port>`, ignoring the
   node's registered `host`. The node must be on the controller host or share
   its network namespace. Direct node URLs and the node's `stream_url` are the
   fallback for a remote node, but the dashboard proxy path then fails. This is
   a real Gate 2 demo constraint, not something this harness can fix.
7. **Node bearer tokens and the controller proxy.** If `--api-token` /
   `OZMA_API_TOKEN` is set, direct checks work with `OZMA_API_TOKEN`, but the
   unmodified controller proxy does not send it and receives 401. Do not set a
   node token for controller-proxied Gate 2 validation unless the controller
   is changed (out of scope here).
8. **Input defaults and scope.** The default VM chain tries `uinput` first; use
   `--input-provider qmp` to guarantee guest injection through QMP. uinput
   needs `/dev/uinput` and group/udev access. Keyboard/pointer only; touch,
   pen, horizontal wheel, and clipboard are rejected rather than faked. Text
   uses `--keyboard-layout us|uk|de|fr` (default `us`); dead-key-only,
   IME/compose, and non-ASCII characters without a direct key are typed 400
   before any event.
9. **Power semantics.** QMP `stop` is a pause, not a destroy; force-off falls
   back to ACPI powerdown. `LibvirtPower` shells out to `virsh`. The harness
   never invokes a power action, so action parity remains manual and must use a
   disposable VM.
10. **Audio is not implemented.** `/api/v1/connection.audio` is
    `"not-implemented"` and `/audio/*` returns typed 501s. There is no per-VM
    sink routing, room correction, or WebRTC audio in this gate.
11. **Registration is one-shot.** There is no direct-registration retry if the
    controller is down at node startup; restart the node. Heartbeat is only
    started when `--register-url` is set.
12. **HLS first-manifest delay.** `hls:"running"` requires `stream.m3u8` to
    exist. A freshly started pipeline can be `failed` for a few seconds with a
    "manifest has not been written yet" error until the first segment lands.
13. **No native D-Bus capture, full provisioning, or controller regression.**
    D-Bus framebuffer capture, libvirt device creation beyond the opt-in XML
    command-line block, evdev/PulseAudio provisioning, and the controller
    pytest/demo/clean-host runs are still outside this harness.

## 8. Evidence and the Gate 2 claim

Record every run under a dated directory (for example
`artifacts/gate2/2026-11-24/`) or attach the files to the parity row. A complete
row needs at least:

| Row | Evidence |
|---|---|
| Discovery/registration | `run-gate2.sh` report + `GET /api/v1/nodes` JSON + node registration/heartbeat log |
| Connection surface | `GET /api/v1/connection` JSON for the running node |
| Power | Node and controller `/power/state` responses |
| Input key/mouse | Direct and controller responses + guest screenshot/video showing the key and pointer |
| HLS | Two playlist fetches showing advance + controller playlist + `ffprobe` segment output + player screenshot |
| WebRTC | Browser offer JSON + node answer + controller answer + browser media screenshot/`webrtc-internals` |
| Dashboard proxy | Screenshot of the unmodified controller dashboard with the Rust node, live view, and input |
| Cleanup | Process list after shutdown and node shutdown log |

> **Do not claim Gate 2 from this document alone.** A harness `PASS`, a code
> inspection, or a unit test is not a Gate 2 row. The integration pass must
> attach the captured artifacts and then update
> [`parity-matrix.md`](parity-matrix.md) and
> [`plan-execution-status.md`](plan-execution-status.md). This document does
> not change those files.

## 9. Troubleshooting

| Symptom | Likely cause / action |
|---|---|
| Harness 401 from node | `OZMA_API_TOKEN` is set but missing/wrong; set it for direct checks, or unset it for controller-proxy checks. |
| Controller 401 | Controller auth is enabled; set `OZMA_CONTROLLER_TOKEN` or use the controller's normal login. |
| Controller lists no node | Node registration failed or the controller restarted; check the node log, then restart the node. |
| Controller power/HLS/WebRTC proxy 502/503 | Node is not on `127.0.0.1` from the controller's perspective; use the same host/network namespace, or validate directly against the node. |
| `hls:"failed"` | Read `hls_error`: no capture source, ffmpeg missing, no `libx264`, bad output directory, or the manifest has not appeared yet. |
| `hls:"not-configured"` | Start the node with `--hls` and keep `video` in `--cap`. |
| HLS playlist 404 but `hls:"running"` | The manifest vanished or the output directory changed; check the node log and `/tmp` permissions. |
| Playlist does not advance | Capture is stalled or the segment interval is longer than the wait; check QMP screendumps and node logs. |
| `webrtc:"not-configured"` | No `--webrtc-helper`; start with the helper program/script. |
| `webrtc:"failed"` | Read `webrtc_error`; check the helper script, GStreamer elements, socket path, and node log. |
| `/webrtc/offer` 503 | Helper not running or did not answer; check `webrtc_error`. |
| `/webrtc/bitrate` 502 | Expected with the current worker; it does not acknowledge bitrate changes. |
| `power: {"available":false}` | No QMP socket or libvirt domain configured. |
| `input` 503 | No input sink; check `--input-provider` and the connection `input` object. |
| `input.injecting:false` / `sink:"recording"` | The node is recording only; fix uinput/QMP permissions and restart. |
| ffmpeg `Unknown format 'vnc'` | The ffmpeg build lacks the VNC demuxer; use QMP capture or install a VNC-enabled build. |
| QMP socket permission denied | Run the node as a user that can connect to the QEMU socket, or adjust the QEMU socket mode/ownership outside this harness. |
