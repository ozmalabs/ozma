# Phase 4: native WebRTC helper integration report

Date: 2026-11-26 (Australia/Sydney)
Checkout: `/home/matt/ozma/ozma`
HEAD: `336c3c7` (native audio follow-up uncommitted; original integration was
validated at `5a33964`)
Node binary: `target/debug/ozma-node` 0.1.0, built from this tree.
Native helper: `ozma-webrtc/target/release/ozma-webrtc-native` 0.1.0.
Peer: GStreamer `webrtcbin` 1.28.9 + `avdec_h264` via
`scripts/gate2/webrtc-client.py` (original run) and
`scripts/webrtc/native-interop-client.py` with `opusdec` (audio follow-up).

## Verdict

The native Rust helper is an **explicit, opt-in backend** of `ozma-node` and
the Rust installer. A node started with `--webrtc-native` spawned the native
binary with the node's existing helper argv (no script), reported
`webrtc_backend: "native"`, and streamed **242 decoded H.264 frames at
640x480** to a real GStreamer peer. A second concurrent node reached
**183 frames at 800x600**.

**Audio follow-up:** the native helper now carries real Opus when
`--audio-source` is supplied. A direct helper + GStreamer run decoded **406
Opus packets into 406 PCM buffers** while **216 H.264 frames** kept arriving
over the same BUNDLE. The original node-integration run above predates this
audio work and therefore shows an `inactive` audio m-line and 0 audio buffers;
it remains accurate as history. `ozma-node` still classifies the native backend
as video-only in its status contract, so `/api/v1/connection` continues to
report `webrtc_audio_configured: false` even when audio flows. That status
integration is the remaining gap, not the media path.

The external GStreamer/Python helper remains the default when the operator
configures it. The node never substitutes the native binary for a configured
Python helper, and no helper at all still reports `not-configured`.

## 1. CLI and backend selection

New flag:

```text
--webrtc-native        env OZMA_WEBRTC_NATIVE
```

Behaviour (`ozma-node/src/webrtc.rs::{select_helper,resolve_native_helper_in}`):

| Configuration | Result |
|---|---|
| no `--webrtc-helper`, no `--webrtc-native` | `webrtc: "not-configured"`, no process |
| `--webrtc-native`, no helper path | sibling `ozma-webrtc-native` next to the node binary, then `PATH` |
| `--webrtc-native --webrtc-helper PATH` | `PATH` is spawned as configured; backend `native` |
| `--webrtc-helper ozma-webrtc-native` | same native resolution, backend `native` |
| `--webrtc-helper python3 --webrtc-helper-script worker.py` | external backend and script unchanged |
| native selected but binary missing | typed `NativeHelperNotFound`; node stays up and reports `failed` + `native` |

* Auto-detection happens **only** after explicit selection. There is no
  implicit Python default and no silent native substitution.
* `--webrtc-helper-script` is dropped for native with a warning. The helper is
  spawned with the same argv the Python worker receives: `--sock`, `--shm`,
  `--width`, `--height`, `--fps`, optional `--audio-source/--audio-backend`,
  `--stun-server`, and `--turn-*`.
* An explicit path containing a directory component is honoured as-is; a bare
  name is searched next to the node and then on `PATH`.

## 2. Status contract

`/api/v1/connection` gained an additive field next to the existing token:

```json
{
  "webrtc": "running",
  "webrtc_backend": "native",
  "webrtc_error": null,
  "webrtc_audio_configured": false,
  "webrtc_audio_source": null,
  "webrtc_geometry": {"width": 640, "height": 480, "scale_to_fit": false, "fitted": false}
}
```

* `webrtc` keeps its old `running`/`failed`/`not-configured` values.
* `webrtc_backend` is `native`, `external`, or `not-configured`.
* The node's status contract still classifies native as video-only:
  `webrtc_audio_configured` is `false` and `webrtc_audio_source` is `null` even
  when an audio source is passed to the helper. This is now conservative rather
  than a helper limitation: the native helper streams Opus when its source
  opens. Updating the node's `WebRtcBackend` capability/status and its tests is
  a separate integration change and was not made here.
* Multi-console status carries `backend` per console and on the summary.
  `--multi-console-webrtc` still derives one socket/shm per console and clones
  the selected backend into each helper config.

A failed native selection is honest rather than a false `not-configured`:

```text
webrtc = failed
webrtc_backend = native
webrtc_error = WebRTC unavailable: native WebRTC helper `ozma-webrtc-native`
               was not found next to `/home/matt/ozma/ozma/target/debug/ozma-webrtc-native`
               or on PATH; install it with scripts/install-rust.sh or pass
               --webrtc-helper <path>
```

That exact state was observed on a node started before the sibling symlink was
created.

## 3. Packaging

`scripts/install-rust.sh`:

* Builds the native helper with
  `cargo build --release --manifest-path ozma-webrtc/Cargo.toml`. The crate
  remains outside the root workspace and keeps its own `Cargo.lock`; the root
  workspace/lockfile is untouched.
* Installs it as `<bin-dir>/ozma-webrtc-native` (next to `ozma-node`, which is
  exactly where sibling detection looks).
* Defaults to installing the helper; `--no-webrtc-native` opts out.
  `--no-helper` still controls only the Python worker.
* Dry-run evidence:

  ```text
  webrtc-rust: /tmp/opencode/ozma-install-check/bin/ozma-webrtc-native (video-only)
  Building native WebRTC helper (cargo build --release --manifest-path .../ozma-webrtc/Cargo.toml)
  DRY   install -m 0755 .../ozma-webrtc/target/release/ozma-webrtc-native .../bin/ozma-webrtc-native
  ```

  With `--no-webrtc-native`, the same run prints `webrtc-rust: skipped` and does
  not invoke the helper build or install.
* `scripts/systemd/env/ozma-node-vm.env.example`,
  `scripts/systemd/env/ozma-node-desktop.env.example`, and
  `scripts/systemd/README.md` document `OZMA_WEBRTC_NATIVE=true` and state that
  native is video-only. Units are unchanged: the node reads the environment
  file and spawns the helper itself.

## 4. Tests

Required commands:

```text
cargo test -p ozma-node --lib
  -> 876 passed, 0 failed, 13 ignored

cargo check -p ozma-node --all-targets
  -> Finished (3 pre-existing ozma-pipewire warnings only)

cargo test --manifest-path ozma-webrtc/Cargo.toml
  -> 45 unit tests passed
  -> tests/helper_lifecycle.rs: 1 passed

rustfmt --check on every modified Rust file
  -> clean
```

Audio follow-up helper coverage (all in `ozma-webrtc`):

* offered Opus payload type reuse and the `sendonly`/`inactive` answer plan;
* target-token parsing (`bare`, `object:`, `monitor:`, `node:`, `default`) with
  rejection of empty/control-character names;
* 44.1 kHz mono -> 48 kHz stereo resampling and unsupported-format rejection;
* exact 960-sample 20 ms assembly, PTS cadence, partial flush, and the bounded
  pending-sample drop counter;
* three real libopus packets decoded back to 960 samples each, plus a
  wrong-sized-frame rejection;
* bounded packet queue drop accounting and an audio start/counter/shutdown
  lifecycle test.

New node coverage:

* `native_argv_omits_the_script_even_when_one_is_configured` — native argv
  starts with `--sock` and never contains the worker script.
* `helper_selection_keeps_the_external_script_and_defaults_to_not_configured` —
  no helper stays `NotConfigured`; external keeps its script.
* `native_selection_prefers_the_sibling_binary_and_drops_the_script` — sibling
  resolution and `--webrtc-helper ozma-webrtc-native` both choose `Native`.
* `native_selection_uses_path_and_missing_is_typed` — `PATH` fallback, explicit
  absolute path passthrough, and `NativeHelperNotFound`.
* `native_status_never_claims_audio` — native status/control are video-only even
  with `audio_source: Some(...)`.
* `native_helper_lifecycle_spawns_a_real_binary_without_a_script` — a real
  executable named `ozma-webrtc-native` is spawned through the production
  `SystemHelperSpawner`; the test asserts the recorded argv has no script,
  `running`/`native` status, a real pid, an offer answer, a bitrate ack, and a
  clean stop.
* `connection_reports_native_backend_without_claiming_audio` — the connection
  document reports `webrtc_backend: "native"`, `webrtc_audio_configured: false`,
  and no audio source.
* `native_per_console_config_never_claims_audio` — per-console `ControllerWebRtcFactory`
  keeps the native backend, drops audio claims, and still retains the source on
  the helper config for argv compatibility.

## 5. Live evidence

### 5.1 Node + native helper + GStreamer peer, 640x480

Node (deterministic fake capture source, explicitly selected native backend):

```bash
./target/debug/ozma-node --name native-live --profile vm \
  --capture-provider fake --fake-width 640 --fake-height 480 --fake-fps 30 \
  --http-port 7392 --hid-udp-port 7391 --cap hid,video \
  --webrtc-native --webrtc-width 640 --webrtc-height 480 --webrtc-fps 30 \
  --webrtc-socket /tmp/opencode/native-live/signal.sock \
  --webrtc-shm /tmp/opencode/native-live/frame.bgra
```

Node log (`/api/v1/connection` agreed):

```text
INFO ozma_node: WebRTC helper started pid=Some(1344126)
  socket=/tmp/opencode/native-live/signal.sock shm=/tmp/opencode/native-live/frame.bgra
  width=640 height=480 fps=30 backend="native" audio=None
```

Process argv (no script):

```text
/home/matt/ozma/ozma/target/debug/ozma-webrtc-native
  --sock /tmp/opencode/native-live/signal.sock
  --shm /tmp/opencode/native-live/frame.bgra
  --width 640 --height 480 --fps 30
```

Peer:

```bash
python3 scripts/gate2/webrtc-client.py \
  --node-url http://127.0.0.1:7392 --duration 8 --bitrate 2500000
```

Exact decoded result:

```json
{
  "ok": true,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "answer_candidates": 6,
  "media": {
    "video": {
      "buffers": 242,
      "bytes": 111513600,
      "caps": "video/x-raw, format=(string)I420, width=(int)640, height=(int)480, interlace-mode=(string)progressive, pixel-aspect-ratio=(fraction)1/1, chroma-site=(string)mpeg2, colorimetry=(string)bt601, framerate=(fraction)0/1"
    },
    "audio": {"buffers": 0, "bytes": 0, "caps": null}
  },
  "bitrate": {"ok": true, "body": {"bitrate": 2500000, "ok": true}}
}
```

The helper's answer carried a single video m-line:

```text
m=video 9 UDP/TLS/RTP/SAVPF 96
a=mid:video0
a=rtpmap:96 H264/90000
a=sendonly
```

### 5.2 Offered Opus audio stays inactive

Same node, peer run with `--audio`:

```text
m=video 9 UDP/TLS/RTP/SAVPF 96
a=mid:video0
a=sendonly
m=audio 9 UDP/TLS/RTP/SAVPF 97
a=mid:audio1
a=rtpmap:97 OPUS/48000
a=inactive
```

```json
{
  "ok": true,
  "connection_state": "connected",
  "video_buffers": 153,
  "video_caps": "I420 640x480",
  "audio_buffers": 0,
  "audio_caps": null
}
```

This proves the m-line shape is accepted by a BUNDLE peer when no native audio
source is configured. (This run predates the audio follow-up; see 5.5 for real
Opus.)

### 5.3 Two concurrent native helpers

A second node used the bare native name and a different geometry:

```bash
./target/debug/ozma-node --name native-live-2 --profile vm \
  --capture-provider fake --fake-width 800 --fake-height 600 --fake-fps 30 \
  --http-port 7393 --hid-udp-port 7394 --cap hid,video \
  --webrtc-helper ozma-webrtc-native \
  --webrtc-width 800 --webrtc-height 600 --webrtc-fps 30 \
  --webrtc-socket /tmp/opencode/native-live/signal2.sock \
  --webrtc-shm /tmp/opencode/native-live/frame2.bgra
```

Both nodes reported `running`/`native`, each with its own helper pid, socket,
and shm (`1,228,800` bytes = 640*480*4 and `1,920,000` bytes = 800*600*4).
Two peers ran concurrently:

```text
node 7392 (640x480): ok=true, connected, ICE completed, 181 video buffers, I420 640x480
node 7393 (800x600): ok=true, connected, ICE completed, 183 video buffers, I420 800x600
```

This is two independent node/helper processes, not the D-Bus per-console
manager. The per-console manager requires independently capturable D-Bus
consoles, which the fake provider does not expose; its native config/status is
covered by unit tests and clones the same backend into every helper.

### 5.4 Environment-variable selection

`OZMA_WEBRTC_NATIVE=true` with no `--webrtc-native` spawned the sibling helper
and reported `webrtc: running`, `webrtc_backend: native`,
`webrtc_audio_configured: false`. A deliberate 320x240 capture with the default
640x480 helper produced the existing typed dimension failure while still
reporting `webrtc_backend: native`.

### 5.5 Native Opus follow-up (direct helper, real packets)

This run validates the helper media path independently of the node's
conservative status field. The source was a disposable null sink fed by a real
440 Hz tone; no default device was changed:

```bash
pactl load-module module-null-sink sink_name=ozma-webrtc-audio-test \
  rate=48000 channels=2
gst-launch-1.0 -q audiotestsrc is-live=true wave=sine freq=440 volume=0.25 \
  ! audio/x-raw,format=S16LE,rate=48000,channels=2 \
  ! pulsesink device=ozma-webrtc-audio-test sync=false

./ozma-webrtc/target/release/ozma-webrtc-native \
  --sock /tmp/opencode/native-audio/signal.sock \
  --shm /tmp/opencode/native-audio/frame.bgra \
  --width 640 --height 480 --fps 30 \
  --audio-source ozma-webrtc-audio-test.monitor --audio-backend pipewire

python3 scripts/webrtc/native-interop-client.py \
  --sock /tmp/opencode/native-audio/signal.sock \
  --shm /tmp/opencode/native-audio/frame.bgra \
  --width 640 --height 480 --fps 30 --duration 8 --min-frames 30 \
  --audio --audio-pt 111 --min-audio-frames 20 --bitrate 2500000
```

Exact decoded result (client exit 0):

```json
{
  "ok": true,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "audio_offered": true,
  "decoded_video": {"buffers": 216, "caps": "I420 640x480"},
  "decoded_audio": {
    "packets": 406,
    "packet_bytes": 120493,
    "buffers": 406,
    "bytes": 1559040,
    "caps": "audio/x-raw S16LE 48000 Hz 2 ch"
  },
  "trickle": {"ok": true},
  "bitrate": {"ok": true, "bitrate": 2500000}
}
```

The answer carried Opus PT 111 `sendonly` next to video; the helper logged no
audio queue drops (`packets_encoded=1031 packets_written=1031
packets_dropped_full=0` across the test sessions). The Pulse backend
(`--audio-backend pulse`, `parec`) decoded another 204 packets into 204 PCM
buffers while video stayed connected. With no source, or with a misspelled
PipeWire target, the answer stayed `inactive`, audio decoded 0 packets, and
video kept arriving; the helper logged the refusal instead of falling back to
the default source.

## 6. Cleanup

* Both nodes received SIGTERM, logged `shutdown requested` and
  `Ozma node shutting down`, and exited.
* Both native helpers exited with their parent. No `ozma-node` or
  `ozma-webrtc-native` process remains.
* Both Unix sockets and shm files were removed; no listeners remain on
  7392/7393.
* The temporary `target/debug/ozma-webrtc-native` sibling symlink was removed.
* No QEMU VM was needed for these runs; the deterministic fake capture source
  wrote real frames to the node's shm.
* For the audio follow-up, all helpers received SIGTERM and logged final
  counters; the disposable null sink, its tone process, and `parec` children
  were removed, and the default sink/source were never changed.

## 7. Remaining gaps

1. **Node status still reports native as video-only.** The helper streams Opus
   when a source is configured, but `ozma-node`'s backend capability/status
   contract and its tests still say video-only. This pass deliberately did not
   widen that contract; only the helper changed.
2. **`--multi-console-webrtc` was not exercised live with two D-Bus consoles**
   on this host. The manager clones the backend and derives per-console
   socket/shm paths; only config/status unit tests and two independent helper
   processes were validated here.
3. **TURN is still not live-verified** for the native helper (host/STUN paths
   were exercised in the prototype report).
4. **No browser matrix** was rerun; the live peer is GStreamer. The Opus
   `sendonly`/`inactive` shapes are designed for browsers but not
   browser-tested in this pass.
5. **The checkout was not frozen.** It contains pre-existing parallel edits
   outside this task (controller, ozma-api, other crates); only the files listed
   below were changed here. Nothing was committed.
6. **Static SDP H.264 profile-level-id**, single video track, and no
   RTX/RTCP-stat assertion remain as documented in the prototype report.

## 8. Files changed by this integration

```text
ozma-node/src/webrtc.rs              backend enum, selection, argv/status
ozma-node/src/multi_webrtc.rs        per-console backend/status, no native audio
ozma-node/src/main.rs                --webrtc-native, selection, logging
ozma-node/src/controller_compat.rs   connection.webrtc_backend
ozma-node/tests/parity.rs            new config field
scripts/install-rust.sh              optional native build/install
scripts/systemd/README.md            installer/unit docs
scripts/systemd/ozma-node@.service   native opt-in comment
scripts/systemd/ozma-node-vm@.service native opt-in comment
scripts/systemd/env/*.env.example    native opt-in examples
ozma-webrtc/README.md                explicit native selection + audio
ozma-webrtc/Cargo.toml               audiopus + path ozma-pipewire dependency
ozma-webrtc/src/audio.rs             capture/resample/Opus track (new)
ozma-webrtc/src/cli.rs               audio flag docs/backend tokens
ozma-webrtc/src/main.rs              AudioConfig wiring
ozma-webrtc/src/server.rs            audio track/transceiver, sendonly/inactive
scripts/webrtc/native-interop-client.py  Opus packet/PCM counters
docs/phase4-native-webrtc-report.md  prototype + audio evidence
docs/phase4-native-webrtc-integration-report.md  this report
```

`docs/parity-matrix.md`, `docs/plan-execution-status.md`, `ozma-node` source,
`ozma-usb`, `ozma-mesh`, `ozma-multiseat`, `ozma-audio`, `ozma-softnode`, and
the Python controller were not edited for the audio follow-up. `ozma-node`
still forwards the audio argv exactly as before.
