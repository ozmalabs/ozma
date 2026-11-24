# Gate 2 final end-to-end report

**Runtime commit:** `ef99c9ec9eb88c22374a82192c2024f61438ab5d` (`ef99c9e`, `feat(gate2): advanced D-Bus, layouts, 4K/CBR, and token UI`)

**HEAD at run start:** `ef99c9e`; the only working-tree changes then were four pre-existing docs edits (`display-client.md`, `routing/audio.md`, `routing/endpoints.md`, `routing/formats.md`).

**HEAD at report time:** `2d7b226491fc7de06020f0f50db316496a15ee1f` (`2d7b226`, `fix(ci): authenticate buf setup action`), with 25 modified/untracked files from concurrent sessions. See [Provenance and the frozen-tree caveat](#provenance-and-the-frozen-tree-caveat).

**Verdict:**

| Claim | Result |
|---|---|
| Consolidated Gate 2 scenario against `ef99c9e` (real Alpine guest, unmodified controller copy, Rust node, HLS, input, WebRTC, codec, audio, force-off) | **PASS**; every required row has captured evidence |
| Gate 2 against current HEAD `2d7b226` | **NOT CLAIMED**; not run |
| “Frozen-tree” Gate 2 | **NOT CLAIMABLE**; the repository moved to two new commits and had tracked source edits during this run |

This run made **no code changes and no commits**. It created only this report. All runtime processes, the temporary venv, the temporary controller copy, QEMU, the null sink, and the evidence root were removed after the evidence below was captured.

---

## 1. Provenance and the frozen-tree caveat

This was requested as a final validation of a committed, frozen tree. The tree was not frozen while the scenario ran:

```text
HEAD at start : ef99c9e  feat(gate2): advanced D-Bus, layouts, 4K/CBR, and token UI
commits added : b3eb85c  fix(ci): skip timing-sensitive keyframe test and skip cross strip
                2d7b226  fix(ci): authenticate buf setup action
HEAD at end   : 2d7b226
```

Concurrent sessions also rebuilt `target/debug/ozma-node` at 20:02 and modified tracked controller, node, worker, and test sources. That is why this report distinguishes **runtime commit** from **current HEAD**:

- The controller used by every row was a `/tmp` copy made before the tree moved. Its `controller/api.py`, `controller/main.py`, `controller/static/dashboard.html`, and `controller/stream.py` hashes exactly match `git show ef99c9e:<path>`.
- The node used by the main scenario was built at 19:52 from the then-clean (docs-only-dirty) `ef99c9e` tree. A concurrent rebuild replaced the path at 20:02, so the check-node and media checks were **re-run** from an explicit `git archive ef99c9e` build at `$ROOT/orig-target/debug/ozma-node` (SHA-256 `6636c839b5b0b3d51b90252cb24210319d1782d49c9de0499ea3309bd2435fe5`).
- The WebRTC worker was modified concurrently at 19:58, after the main node start but before the first successful media test. The authoritative WebRTC/dashboard media evidence was therefore re-run with `$ROOT/src-ef99c9e/softnode/gst_webrtc_worker.py` (SHA-256 `209769a7438244fd4d9e8338c62e040b4682a06df207e617f4bed3587d138d0f`, identical to `git show ef99c9e:softnode/gst_webrtc_worker.py`).
- No command in this run downloaded an ISO, VM, or package from the network. The temporary controller venv was installed only from a wheelhouse reconstructed from the local pip HTTP cache, with system-site packages for already-installed dependencies.

**Consequence:** the scenario evidence is valid for `ef99c9e`, not for `2d7b226` or any later tree. A frozen-tree Gate 2 claim cannot be made from this run.

---

## 2. Environment and versions

Evidence: `$ROOT/evidence/versions.txt` (SHA-256 `04cde4c5db71ddcdcac4a95ff08d876f32fd10fa9d5ed3cc80bd54aa5e6f5d0f`).

| Component | Version / identity |
|---|---|
| Host | `Linux 7.3.5-2-cachyos x86_64 GNU/Linux` |
| QEMU | 11.1.3, KVM (`accel=kvm:tcg`) |
| Guest | Alpine Linux 3.21 live, kernel `6.12.13-0-virt` |
| ISO | `/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso`, SHA-256 `f28171c35bbf623aa3cbaec4b8b29297f13095b892c1a283b15970f7eb490f2d`, 66,060,288 bytes, opened read-only |
| Guest kernel/initramfs | extracted read-only from the ISO; kernel SHA-256 `e1d7a3cdae9a4a62ed629b90a9955754f676a339bc28176aa593f8f029c35e3c` |
| Rust node | `ozma-node 0.1.0`, ef99c9e build SHA-256 `6636c839…435fe5` |
| Rust toolchain | `rustc 1.95.0`, `cargo 1.95.0` |
| Controller | `ef99c9e` copy, Python 3.14.10 |
| Controller venv | FastAPI 0.164.0, Uvicorn 0.54.0, Pydantic 2.14.2, Strawberry 0.341.3, aiohttp 3.13.5, PyNaCl 1.6.2, toml 0.10.2, httpx 0.28.1, protobuf 7.36.1 |
| ffmpeg/ffprobe | n9.1 |
| GStreamer | 1.28.9; `webrtcbin`, `rtph264pay`, `x264enc`, `opusenc`, `pulsesrc` present |
| Browser | Google Chrome for Testing 145.0.7632.77, headless via Playwright 1.58.0 |
| Audio | PipeWire 1.6.11 / PulseAudio 15.0.0 compatibility; disposable null sink `ozma-g2-final` |

Ports were unique to this run: controller `42580`, node HTTP `52882`, node HID/UDP `54831`, node QUIC `52884`.

---

## 3. Exact startup commands

### 3.1 QEMU

`$ROOT = /tmp/opencode/ozma-gate2-final-20261124-195329`

```bash
ROOT=/tmp/opencode/ozma-gate2-final-20261124-195329
ISO='/home/matt/ozma/ozmanew/demo/cache/alpine-virt-3.21.3-x86_64.iso'
xorriso -osirrox on -indev "$ISO" \
  -extract /boot/vmlinuz-virt "$ROOT/qemu/vmlinuz-virt" \
  -extract /boot/initramfs-virt "$ROOT/qemu/initramfs-virt"

pactl load-module module-null-sink sink_name=ozma-g2-final \
  sink_properties=device.description=ozma-g2-final rate=48000 channels=2

qemu-system-x86_64 \
  -name ozma-gate2-final \
  -machine pc,accel=kvm:tcg -cpu host -smp 4 -m 2048 \
  -nodefaults \
  -kernel "$ROOT/qemu/vmlinuz-virt" \
  -initrd "$ROOT/qemu/initramfs-virt" \
  -append "console=ttyS0 console=tty0 modprobe.blacklist=virtio_gpu modules=loop,squashfs,sd-mod,usb-storage,iso9660,snd-hda-intel alpine_dev=cdrom" \
  -drive "file=$ISO,media=cdrom,readonly=on" \
  -device VGA \
  -device virtio-gpu-pci \
  -device qemu-xhci,id=xhci \
  -device usb-kbd,bus=xhci.0 \
  -device usb-tablet,bus=xhci.0 \
  -audiodev "pa,id=snd0,out.name=ozma-g2-final" \
  -device intel-hda \
  -device hda-duplex,audiodev=snd0 \
  -display none \
  -serial "file:$ROOT/qemu/serial.log" \
  -qmp "unix:$ROOT/qemu/qmp.sock,server=on,wait=off" \
  -pidfile "$ROOT/qemu/qemu.pid" \
  -daemonize
```

The guest was reached at `localhost login:` after 5 s. VGA framebuffer was 1280x800. `virtio-gpu-pci` was present on the PCI bus (`query-pci` in `$ROOT/evidence/qmp-initial.json`) but its driver was blacklisted so the VGA console remained the framebuffer that QMP `screendump` captures. Without that blacklist, `virtio_gpu` became `fb0` at 0.71 s and the VGA screenshot froze at early boot while the serial console continued; that was an environment/display-selection issue, not a Gate 2 code failure.

### 3.2 Controller

```bash
python3 -m venv --system-site-packages "$ROOT/venv"
"$ROOT/venv/bin/python" -m pip install --no-index \
  --find-links /tmp/ozma-g2-wheelhouse -r controller/requirements.txt
cp -a controller/. "$ROOT/controller/"
cd "$ROOT/controller"
OZMA_AUDIO=0 PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 \
  "$ROOT/venv/bin/python" main.py --host 127.0.0.1 --port 42580
```

`GET /health` returned `{"status":"ok"}`. No controller source change was made.

### 3.3 Rust node

```bash
RUST_LOG=info,ozma_qmp=debug,ozma_input=debug "$ROOT/orig-target/debug/ozma-node" \
  --name ozma-gate2-final \
  --profile vm \
  --qmp-socket "$ROOT/qemu/qmp.sock" \
  --input-provider qmp \
  --power-provider qmp \
  --keyboard-layout us \
  --http-port 52882 \
  --hid-udp-port 54831 \
  --cap hid,video,audio,power \
  --machine-class server \
  --hls --hls-dir "$ROOT/node/hls" --hls-fps 5 --hls-bitrate-kbps 1000 \
  --ffmpeg-bin /usr/bin/ffmpeg \
  --webrtc-helper /usr/bin/python3 \
  --webrtc-helper-script "$ROOT/src-ef99c9e/softnode/gst_webrtc_worker.py" \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 15 \
  --webrtc-socket "$ROOT/node/webrtc.sock" \
  --webrtc-shm "$ROOT/node/webrtc-shm" \
  --webrtc-audio-source ozma-g2-final.monitor \
  --webrtc-audio-backend pulse \
  --audio-router pipewire \
  --audio-sink ozma-g2-final \
  --audio-track-root "$ROOT/node/tracks" \
  --register-url http://127.0.0.1:42580 \
  --register-host 127.0.0.1 \
  --heartbeat-interval 5
```

No Python soft node was running. The only soft-node-shaped process visible at cleanup was another session's unrelated `ozma-node --name nat-ice` on ports 54001/54002; it was left untouched.

---

## 4. Consolidated PASS/FAIL/SKIP matrix

| Gate 2 row | Result | Evidence |
|---|---|---|
| Discovery / registration / heartbeat | **PASS** | Controller list/detail JSON, activation `{"ok":true,...}`, heartbeat every 5 s in node log |
| Dashboard node card | **PASS** | Playwright screenshot of the unmodified dashboard card with HID VIDEO AUDIO POWER and PipeWire sink; activation changed the card to `ACTIVE` |
| `/api/v1/connection` | **PASS** | `hls:"running"`, `webrtc:"running"`, `audio:"running"`, `power:{available:true,status:"running"}`, `input:{sink_kind:"qmp",available:true,injecting:true}` |
| HLS playlist advance (5 fps, 1 s segments) | **PASS** | `MEDIA-SEQUENCE:36` → `41` in 5 s; `#EXT-X-TARGETDURATION:1`; `#EXTINF:1.000000`; controller proxy playlist matches |
| HLS segment decode | **PASS** | `ffprobe`: `codec_name=h264`, 1280x800, `duration=1.000000`; node and controller segment hashes identical |
| `/display/snapshot` node + controller proxy | **PASS** | Both HTTP 200 `image/jpeg`, 1280x800, byte-identical SHA-256 |
| `/input/type` into guest | **PASS** | `root\n` and a command typed through the node; exact marker and `uname -a` output appeared on the guest serial console; mouse move/click returned `{"ok":true}` |
| Power state / pause / resume / reset | **PASS; pause/resume route gap closed after this run (§8)** | Node and controller state `running`; QMP pause froze two PPMs byte-identically; QMP resume returned `running`; controller reset rebooted the guest (serial 28,446 → 56,664 bytes). At run time the node had **no pause route** and `/power/stop` was hard force-off, so pause/resume were QMP-level; §8 adds and live-tests `/power/pause` and `/power/resume`. |
| WebRTC through the controller, real GStreamer peer | **PASS** | 9 offer + 9 answer candidates, `connected` / ICE `completed`, 151 H.264 buffers at 1280x800 15 fps, 520 Opus buffers at 48 kHz stereo, bitrate 4 Mbps acknowledged |
| WebRTC through the controller, headless dashboard | **PASS** | `connected`, 1280x800 video, 91 decoded frames / 15 fps, 314 audio packets, status `WebRTC 90f 15fps 442KB` |
| `/codec` GET/POST through controller | **PASS** | GET advertised h264/hevc and software/vaapi; POST `{"codec":"h264","hw_accel":"software","bitrate":"1500k"}` returned `applied:true`, `pending_restart:false`, `encoder:"libx264"` after a real HLS restart; invalid `vp8` returned typed 400 |
| `/audio/*` through controller | **PASS** | Apply, play, stop, remove through the controller all returned `ok:true`; 523 Hz reference captured from the monitor at peak −21.1 dBFS |
| Guest audio | **SKIP (optional)** | Guest exposed HDA `card 0 [Intel]` and `/dev/snd` (`controlC0`, `pcmC0D0c/p`), but Alpine live has no playback tool; host-side `/audio/*` path passed |
| `scripts/gate2/check-node.sh` | **PASS** | `12 passed, 0 failed, 0 warnings, 0 skipped`, exit 0, against the explicit `ef99c9e` node build |
| Hard force-off + QEMU exit | **PASS** | Controller `POST /power/force-off` → HTTP 200 `{"action":"force-off","ok":true}`; QEMU exited in 200 ms; node then honestly returned typed 502 |
| Cleanup | **PASS** | Node/controller exited on SIGINT, no child ffmpeg/helper remained, ports free, null sink unloaded, temp root removed |

---

## 5. Evidence detail

Artifact root (removed after this report): `/tmp/opencode/ozma-gate2-final-20261124-195329/`.

Full manifest, including every SHA-256: `$ROOT/evidence/artifact-manifest.txt` (SHA-256 `1e0a373482626f34b40d0bd5e909a4f438d0c9f18e9019282222a4e4487992cc`).

### 5.1 Registration, discovery, dashboard

- Controller list (`controller-nodes-1.json`, SHA-256 `adc3c4fd…d3a036`): one node, `id=ozma-gate2-final._ozma._udp.local.`, `port=54831`, `api_port=52882`, `stream_url=http://127.0.0.1:52882/stream/stream.m3u8`, capabilities `hid,video,audio,power`, `machine_class=server`.
- Activation (`controller-activate.json`, SHA-256 `19107b59…a4a8e`): `{"ok":true,"active_node_id":"ozma-gate2-final._ozma._udp.local."}`.
- Node log shows registration accepted and heartbeat every 5 s; controller survived the full scenario after being started with `setsid`.
- Dashboard screenshots (headless Chrome 145):
  - `$ROOT/rerun-media/evidence/dashboard-node-card.png` (SHA-256 `58f089fa…8af36e`)
  - `$ROOT/rerun-media/evidence/dashboard-activated.png` (SHA-256 `1cb775ff…68f969`)
  - `$ROOT/rerun-media/evidence/dashboard-webrtc.png` (SHA-256 `45671c50…ed9c`)
- Two console 404s were observed for static assets (the known logo/favicon path issue). No page errors, no failed requests, and no failing API requests. This is cosmetic, not a Gate 2 media failure.

### 5.2 HLS

```text
playlist-1  #EXT-X-MEDIA-SEQUENCE:36   #EXTINF:1.000000
playlist-2  #EXT-X-MEDIA-SEQUENCE:41   #EXTINF:1.000000
controller  #EXT-X-MEDIA-SEQUENCE:41   #EXTINF:1.000000
```

- `$ROOT/evidence/playlist-1.m3u8` (SHA-256 `feb5d20d…6727cc`)
- `$ROOT/evidence/playlist-2.m3u8` and `controller-playlist.m3u8` (both SHA-256 `9cf14407…bab2a2`)
- Segment `seg_00041.ts` (SHA-256 `736f75fa79a79db1c7a35ec7e119774dcbe7d0d9bf55e3b2e4125a39465599d6`), node and controller copies identical, 119,004 bytes.
- `ffprobe`: `codec_name=h264`, `width=1280`, `height=800`, `duration=1.000000`.
- After the codec override restarted HLS, the playlist restarted at `MEDIA-SEQUENCE:1` and the new segment still probed as H.264 1280x800.

### 5.3 Snapshot

- Before input: node and controller snapshots both 720x400 JPEG, SHA-256 `d08ec0f9…7bb73`.
- Final: node and controller snapshots both 1280x800 JPEG, SHA-256 `19551581bce82d7731699b43371bb3a5732eba23c819445e957690d4207eafcb`.
- Controller proxy and node bytes were identical.

### 5.4 Guest-visible input

`POST /input/type {"text":"root\n"}` returned `{"events":10,"ok":true}`. The VGA OCR then showed `localhost login: root` and the Alpine MOTD/shell prompt.

The command typed through the node was:

```text
echo OZMA-GATE2-FINAL-SERIAL-MARKER > /dev/ttyS0
uname -a > /dev/ttyS0
```

The serial log received, byte-for-byte:

```text
localhost login: OZMA-GATE2-FINAL-SERIAL-MARKER
Linux localhost 6.12.13-0-virt #1-Alpine SMP PREEMPT_DYNAMIC 2025-02-10 21:47:33 x86_64 Linux
```

Later, a 344-event `/input/type` command wrote `GUEST-AUDIO:`, `/proc/asound/cards`, `/dev/snd`, and `FINAL-E2E-DONE` to `/dev/ttyS0`. The serial log shows `card 0 [Intel]: HDA-Intel`, `controlC0`, `pcmC0D0c`, `pcmC0D0p`, and `FINAL-E2E-DONE`. A mouse move and left click both returned `{"ok":true}`.

### 5.5 Power

- Node and controller state: `{"available":true,"status":"running"}`.
- QMP pause: `{"status":"paused","running":false}`; two PPM screendumps 2 s apart were byte-identical (SHA-256 `e4a6dbb24ea4ef2155114958ed5c9c0aa23afcc46ba0b1e6198cd9f3d7bac3df`), then QMP resume returned `running`.
- Controller reset: HTTP 200 `{"action":"reset","ok":true}`; serial grew 28,446 → 56,664 bytes and returned to a fresh `localhost login:`.
- Hard force-off: controller HTTP 200 `{"action":"force-off","ok":true}`; QEMU PID exited within 200 ms; the node then returned HTTP 502 `code:"qmp-connect"` rather than pretending the VM was still available.

**API gap (closed after this run, §8):** the compatibility power surface had `on`, `off`, `reset`, and `force-off`, but no pause/resume route. `/power/stop` and `/power/stop`-style aliases mapped to hard force-off (`ozma-power/src/action.rs`). Pause/resume therefore had to be exercised through QMP while the node was stopped. That was honest but was not node-route pause parity. A follow-up added distinct `/power/pause` and `/power/resume` actions and live-tested them against a disposable QEMU; see §8.

### 5.6 WebRTC

GStreamer peer through the controller (`webrtc-report.json`, SHA-256 `de3c6c13…f712a`):

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
    "video": {"buffers": 151, "caps": "I420 1280x800 15/1"},
    "audio": {"buffers": 520, "caps": "S16LE 48000 Hz 2 channels"}
  },
  "bitrate": {"ok": true, "body": {"bitrate": 4000000, "ok": true}}
}
```

Headless Chrome dashboard through the controller (`dashboard-result.json`, SHA-256 `0caa19d9…d2011f5`):

```json
{
  "connectionState": "connected",
  "iceConnectionState": "connected",
  "videoWidth": 1280,
  "videoHeight": 800,
  "framesDecoded": 91,
  "framesPerSecond": 15,
  "audio": {"packetsReceived": 314, "bytesReceived": 75914, "jitter": 0.002}
}
```

The dashboard used its real WebRTC-first modal (`/api/v1/streams/{id}/webrtc`), not the HLS fallback. The page was `Ozma — Dashboard`, the node card activated to `ACTIVE`, and the status line read `WebRTC 90f 15fps 442KB`.

### 5.7 `/codec`

- Controller GET before: `current_encoder:"h264"`, `available:["h264","hevc"]`, `backends:["vaapi","software"]`, `applied:false`, `pending_restart:false`.
- Controller POST software H.264 1500k: HTTP 200, `applied:true`, `pending_restart:false`, `encoder:"libx264"`, `fallback:false`, real policy receipt, `hls:"running"`.
- Controller GET after: `current_encoder:"libx264"`, `applied:true`, `pending_restart:false`, stored override receipt.
- Invalid `vp8`: HTTP 400 typed `invalid_request`, `available` capabilities in `details`, no success claim.
- HLS continued after the restart and the next segment was a real H.264 1280x800 segment.

The `pending_restart:true` branch was not triggered in this scenario because HLS was always running during the POST. The response shape and restart path were exercised; the pending case remains covered only by the node's unit tests.

### 5.8 Audio

The controller's room-correction audio routes were used with the disposable null sink:

```text
POST /api/v1/audio/room-correction/apply  {"node_id":..., "profile_id":"gate2-final", "volume":55}
  -> {"ok":true,"backend":"pipewire","profile_id":"gate2-final","sink":"ozma-g2-final"}
POST /api/v1/audio/room-correction/play   {"node_id":..., "track":"gate2-tone.wav", "sink":"ozma-g2-final"}
  -> {"ok":true,"backend":"pipewire","sink":"ozma-g2-final","track":"gate2-tone.wav"}
POST /api/v1/audio/room-correction/stop   -> {"ok":true,"backend":"pipewire"}
POST /api/v1/audio/room-correction/remove -> {"ok":true,"backend":"pipewire","profile_id":"gate2-final"}
```

A `parec` capture of `ozma-g2-final.monitor` during playback measured `n_samples: 262144`, `mean_volume: -25.1 dB`, `max_volume: -21.1 dB`. The raw capture SHA-256 is `1ff09e12…389f`.

The guest exposed an Intel HDA device and `/dev/snd`, but Alpine live carries no `aplay`/`speaker-test`/`pw-play`, so guest-side audible playback was not attempted. That is an optional row and is marked SKIP.

### 5.9 Harness

```bash
OZMA_NODE_URL=http://127.0.0.1:52882 \
OZMA_NODE_ID=ozma-gate2-final._ozma._udp.local. \
OZMA_CONTROLLER_URL=http://127.0.0.1:42580 \
OZMA_GATE2_HLS_ADVANCE_WAIT=6 \
OZMA_GATE2_CONTROLLER_INPUT=1 \
OZMA_GATE2_WEBRTC_SDP_FILE="$ROOT/evidence/webrtc-offer-check.json" \
scripts/gate2/check-node.sh
```

Result against the explicit `ef99c9e` build (`check-node-ef99c9e.log`, SHA-256 `c66bd1f6…3d258`, exit 0):

```text
PASS  /health
PASS  /api/v1/connection
PASS  /power/state
PASS  /stream/stream.m3u8
PASS  HLS playlist advanced within 1s
PASS  /webrtc/offer returned a non-empty answer for the supplied browser offer
PASS  controller /health
PASS  controller lists node
PASS  controller node detail
PASS  controller power proxy
PASS  controller HLS proxy
PASS  controller input proxy (key release)
Summary: 12 passed, 0 failed, 0 warnings, 0 skipped
```

An earlier successful `check-node.sh` run (`check-node-final.log`, SHA-256 `2bf777d7…ec283`) had used the concurrently rebuilt `target/debug/ozma-node`; it is superseded by the explicit `ef99c9e` re-run above.

`qemu-preflight.sh` was not run after the node started because QEMU accepts one QMP client per socket and the node owns the shared client. The node and controller checks above cover the live HTTP surfaces; the preflight is a pre-start check by design.

### 5.10 Cleanup

- Node exited on SIGINT (`Ozma node shutting down`) and reaped its HLS ffmpeg and WebRTC helper.
- Controller exited gracefully on SIGINT (`Application shutdown complete`, `Finished server process`) with no traceback and no orphan capture ffmpeg.
- QEMU exited via controller `force-off`; the PID was gone within 200 ms.
- Null sink modules (`536870916` and the rerun module) were unloaded; `pactl list short sinks` no longer contains `ozma-g2-final`.
- Ports `42580`, `52882`, `54831`, and `52884` were free.
- Process scan for this stack was empty; the unrelated concurrent `nat-ice` processes were left alone.
- The `/tmp` venv, controller copy, wheelhouses, QEMU temp dirs, evidence root, browser profile, and all test screenshots were removed after the manifest and this report were produced.

Post-report verification (2026-11-24T09:10Z): no `qemu-system`/`ozma-node`/`gst_webrtc_worker`/controller process from this run remained; ports 42580, 52882, 54831, and 52884 were free; `pactl list short sinks` no longer contained `ozma-g2-final`; `/tmp/opencode/ozma-gate2-final-*`, `/tmp/ozma-g2-venv-test`, and `/tmp/ozma-g2-wheelhouse` were absent. The only new repository file is this report (`?? docs/gate2-final-e2e-report.md`); no code or commit was made by this run.

---

## 6. Failures, blockers, and doc drift

### 6.1 Blockers to a current/frozen-tree Gate 2 claim

1. **The tree moved during the run.** Two commits and 25 tracked/untracked edits appeared while the scenario was running. Even though the runtime was pinned to `ef99c9e` by an explicit source archive and verified file hashes, this was not a frozen-tree run and current HEAD `2d7b226` was never validated. This is the primary reason not to claim a current-HEAD Gate 2.
2. **No node pause/resume API at run time (closed after this run; §8).** Power state, reset, and hard force-off are real and honest. Pause/resume existed only at QMP level and `/power/stop` was hard force-off. The follow-up added distinct `/power/pause` and `/power/resume` routes with QMP `stop`/`cont` and libvirt `suspend`/`resume`, plus a live disposable-QEMU check.
3. **Guest audio playback is not available in Alpine live.** The HDA device and `/dev/snd` exist, but there is no guest playback tool. The optional row is SKIP; the host `/audio/*` path passes.

### 6.2 Documentation drift observed at run time (all fixed after this run; §8)

- `docs/gate2-validation.md` §7.4 says HLS does not use `delete_segments`, but the current `ozma-node/src/hls.rs` argv includes `delete_segments+append_list+discont_start`.
- `docs/gate2-validation.md` §7.9 and the older guest/power reports describe `/power/stop` as a pause. The current `ozma-power` action map treats `/power/stop` as hard force-off.
- The node startup warning says `POST /codec` only stores an override and does not restart HLS. The live route did restart HLS and returned `applied:true`; the warning is stale.
- `docs/gate2-live-controller-webrtc-report.md` says the controller `/codec` proxy had only fake-node test coverage and had not been repeated live. This run repeated it live through the controller and got an applied restart.
- The controller dashboard still requests two static assets that 404 (known cosmetic logo/favicon issue).

### 6.3 Environment notes

- `virtio-gpu-pci` had to be blacklisted for the guest so `virtio_gpu` would not take over `fb0`; otherwise QMP `screendump` captured a frozen VGA buffer. The device remained present in the QEMU PCI topology. This is a test-harness display-selection constraint, not a node bug.
- QEMU accepts one QMP client per socket. While the node held the shared client, a second QMP connection could not be used for pause; the node had to be stopped for the QMP pause/resume probe.
- The first startup before the VGA/virtio-gpu adjustment produced a 720x400 frame source while the WebRTC helper expected another size; the helper honestly reported the dimension mismatch. A fresh node start after the guest mode settled resolved it. This is the documented “frame dimensions must match” constraint, not a silent fallback.
- Two node processes and the first controller were lost while started with plain `nohup` (likely process-group cleanup by the surrounding session). After starting them with `setsid`, they remained stable through the full run. This is an environment/launcher issue; the application logs showed no traceback or OOM.

---

## 7. Gate 2 claim

**Scenario at `ef99c9ec9eb88c22374a82192c2024f61438ab5d`: PASS.** The unmodified controller copy discovered and activated the Rust VM node with no Python soft node; HLS advanced and decoded; snapshots matched through the controller proxy; `/input/type` reached the Alpine guest and the guest serial console; power state/reset and hard force-off were real; a real GStreamer peer and a headless Chromium dashboard both completed ICE/DTLS and decoded H.264 video plus Opus audio through the controller; `/codec` applied a real HLS restart; `/audio/*` routed and played through a monitor sink; `check-node.sh` passed 12/12; cleanup was complete.

**Frozen-tree/current-HEAD Gate 2: NOT CLAIMED.** The repository was not frozen during the run and current HEAD `2d7b226` was not validated. A subsequent follow-up (§8) closed the pause/resume route gap and fixed the doc/dashboard drift but did not re-run the full Gate 2 scenario. The docs/parity-matrix should cite this run only as evidence for `ef99c9e`, with those caveats visible.

---

## 8. Follow-up after this run (pause/resume API and doc/dashboard fixes)

This follow-up changed only the power API, compatibility routes/tests, static asset paths, and docs; no commit was made.

### 8.1 Power API and action mapping

- `ozma_power::PowerAction` now has distinct `Pause` (`pause`, alias `suspend`) and `Resume` (`resume`) actions. `force-off`/`stop`/`destroy` remain `PowerAction::Stop`; `on`/`start`/`cont` remain `PowerAction::Start`.
- `PowerBackend::pause` has a default typed `unsupported`; `PowerBackend::resume` defaults to `cont`. `QmpPower::pause` sends QMP `{"execute":"stop"}` (monitor pause, not `quit`); `QmpPower::resume` sends `cont`. `LibvirtPower` uses `virsh suspend` / `virsh resume`.
- `POST /power/pause` and `POST /power/resume` return `{"ok":true,"action":"pause"|"resume"}`; `GET /power/state` reports `paused` after pause. `/power/stop` still sends QMP `quit` / `virsh destroy` and is unchanged.
- The controller's existing `POST /api/v1/nodes/{node_id}/power/{action}` proxy forwards both actions with the node bearer token; a controller pytest asserts the exact paths and header.

### 8.2 Live disposable-QEMU evidence (2026-11-24)

QEMU 11.1.3, headless `-nodefaults -display none`, TCG, disposable QMP socket; `ozma-node --power-provider qmp` from this tree. Exact HTTP results:

```text
GET  /power/state   -> {"available":true,"status":"running"}
POST /power/pause   -> {"action":"pause","ok":true}
GET  /power/state   -> {"available":true,"status":"paused"}
POST /power/resume  -> {"action":"resume","ok":true}
GET  /power/state   -> {"available":true,"status":"running"}
POST /power/reset   -> {"action":"reset","ok":true}; state running
POST /power/on      -> {"action":"on","ok":true};    state running
POST /power/off     -> {"action":"off","ok":true};   state running (ACPI, QEMU alive)
POST /power/stop    -> {"action":"force-off","ok":true}
GET  /power/state   -> HTTP 502 {"available":false,"code":"qmp-connect",...}
```

The QEMU process exited after `/power/stop`. Node, QEMU, socket directory, and ports were removed/checked free after the run.

### 8.3 Tests

- `cargo test -p ozma-power -p ozma-node`: 339 passed, 0 failed, 6 ignored (includes new QMP `stop`/`cont` mapping, libvirt `suspend`/`resume` argv, node fake-QMP pause→`paused`→resume→`running`, fake-backend action mapping, and typed unsupported/default-resume coverage).
- `python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py`: 37 passed, including controller pause/resume action propagation and bearer-token cases.

### 8.4 Doc and dashboard fixes

- `docs/gate2-validation.md`: HLS now correctly documented as using `delete_segments+append_list+discont_start`; `/power/stop` documented as hard force-off; pause/resume mapping documented.
- `docs/gate2-live-controller-webrtc-report.md` and `docs/parity-matrix.md`: controller `/codec` proxy now recorded as live-tested through a real Rust node (`applied:true`, `pending_restart:false`, HLS restarted, next segment H.264).
- The stale node startup warning now says `POST /codec` restarts a running HLS pipeline and stores `pending_restart` only when HLS is off.
- `controller/static/dashboard.html`, `index.html`, and `desk3d.html` requested `/static/favicon.ico` / `/static/logo.svg`, but the controller mounts `controller/static` at `/`; the paths are now `/favicon.ico` and `/logo.svg`, so those 404s are gone.
