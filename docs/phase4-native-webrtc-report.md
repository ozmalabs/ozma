# Phase 4: native Rust WebRTC helper report

**Scope:** replace the GStreamer/Python WebRTC helper with a standalone Rust
binary that speaks the node's existing signaling protocol and streams a real
OpenH264-encoded video track over `webrtc-rs`. The helper now also carries an
opt-in native Opus audio track from a PipeWire or Pulse source. This report
covers the prototype and the audio follow-up. The opt-in node integration
(`--webrtc-native`) is documented in
`docs/phase4-native-webrtc-integration-report.md`; native remains non-default.

**Result: PASS for the prototype and native audio.** A real GStreamer
`webrtcbin` peer completed ICE and DTLS against the native helper and decoded
**155 real H.264 frames at 640x480** in the original video run (and 180 with
STUN enabled). In the audio run it decoded **406 real Opus packets into 406
PCM buffers** while **216 H.264 frames** kept arriving at 640x480 over the same
BUNDLE. The answer, trickle-candidate ack, and bitrate ack all match the node's
contract. No frame or packet is claimed without a decoded-buffer count.

| Item | Value |
|---|---|
| Date | 2026-11-26 ~12:38–14:35 AEDT |
| Git HEAD | `336c3c7` (audio work uncommitted) |
| Working tree | dirty with this work plus pre-existing concurrent edits; nothing committed by this work |
| CPU | AMD Ryzen 5 9600X (6C/12T) |
| Rust | 1.95.0 |
| GStreamer peer | 1.28.9 (`webrtcbin`, H.264 `avdec_h264`, Opus `opusdec`) |
| Encoder | OpenH264 0.9.10 (bundled, software, baseline profile) |
| Audio encoder | libopus via `audiopus` 0.3.0-rc.0 (bundled `audiopus_sys`) |
| `webrtc-rs` | `webrtc` 0.11.0 |
| Transport | Unix socket signaling + packed BGRA shared memory |

## 1. What was built

New files (all untracked; not committed):

```text
ozma-webrtc/
  Cargo.toml                 # separate workspace + lockfile
  Cargo.lock
  README.md
  src/main.rs                # CLI, signals, startup
  src/cli.rs                 # node-compatible flag set + validation
  src/encoder.rs             # BGRA -> I420 -> OpenH264 Annex-B
  src/audio.rs               # capture -> resample/reframe -> Opus track
  src/shm.rs                 # exact-length BGRA validation/read
  src/sdp.rs                 # media-section split, candidate append, offered PT
  src/signaling.rs           # JSON messages, acks, bitrate clamp, ICE URLs
  src/server.rs              # webrtc-rs peer connections + signaling server
  tests/helper_lifecycle.rs  # real process: socket, acks, SIGTERM cleanup
scripts/webrtc/native-interop-client.py   # live GStreamer peer + BGRA feeder
                                          # + Opus packet/PCM counters
```

### Why a separate workspace

`webrtc-dtls` requires stable `x25519-dalek ^2`, while the main workspace's
`ozma-crypto` pins `=2.0.0-rc.3`. Cargo cannot unify the two in one lockfile.
`ozma-webrtc` therefore has its own `[workspace]`/`Cargo.lock` and is not a
member of the root workspace. This keeps `ozma-node`'s dependency graph and
build untouched. It also means `cargo test` is run from `ozma-webrtc/`.

### Media path

```text
node -> <shm> packed BGRA (atomic temp+rename)
        |
        v  bounded sync_channel(2), fps tick
  [BGRA -> I420 conversion + OpenH264]  (dedicated OS thread)
        |
        v  bounded mpsc(2), Annex-B access units
  TrackLocalStaticSample (H.264) -> webrtc-rs RTP/SRTP -> N peer connections

--audio-source only --------------------------------------------------------
PipeWire stream  or  parec --device <source>
        |
        v  bounded capture queue (8), dropped-frame counters
  mono/stereo -> stereo, linear resample to 48 kHz
        |
        v  exact 20 ms frames (960 samples/ch), <=4 frames pending
  libopus @ 128 kbps (audiopus) -> bounded packet queue (8)
        |
        v  TrackLocalStaticSample (Opus) -> webrtc-rs RTP/SRTP -> N peers
```

* One video encoder/track and one audio track are shared by all peers; adding a
  peer does not add an encoder. Capture and encoding pause while no peer is
  connected, but the capture stream keeps draining so its bounded queue cannot
  grow.
* Encoding is idle at 0% CPU while no peer is connected.
* A full queue drops the newest frame/packet instead of buffering; counters are
  logged at shutdown.
* Keyframes: one IDR per second, on every new peer, and after a bitrate change.
  OpenH264 scene-change detection can add an IDR on a large visual change.
* Audio packets are one 20 ms Opus frame each. Signaling `bitrate` messages
  change video only; the native audio bitrate is fixed at 128 kbps.
* A named source is checked in the PipeWire graph before opening. A missing
  source is a warning and a video-only helper, never a fallback to the default
  microphone.

### Signaling compatibility

| Node sends | Native helper replies |
|---|---|
| `{"type":"offer","sdp":"..."}` | `{"type":"answer","sdp":"...","session":"<8 hex>"}` |
| `{"type":"add-ice-candidate","session":"...","candidate":"...","sdpMLineIndex":0}` | `{"ok":true,"session":"...","remote_candidates":N}` |
| `{"type":"bitrate","bitrate":<bps>}` | `{"ok":true,"bitrate":<clamped bps>}` |
| invalid offer | `{"type":"error","error":"..."}` |
| invalid candidate/session | `{"ok":false,"error":"..."}` |

* Bitrate is clamped to **500 kbps–50 Mbps**, matching the Python worker.
* Candidates must start with `candidate:`, carry `typ host/srflx/prflx/relay`,
  and contain no control characters.
* The helper reuses the peer's offered H.264 payload type (96 for the repo
  GStreamer client, 103+ for Chrome) and emits local candidates inside the
  answer after a bounded gather. Trickle candidates are also accepted.
* An offered Opus m-line reuses the offered payload type. With a live
  `--audio-source` the answer is **`sendonly`** and carries real Opus; without
  one it is **`inactive`** with a valid mid, rtpmap, and BUNDLE entry.
  Rejecting the m-line without a mid made GStreamer reject the whole BUNDLE
  answer, so the inactive compatibility path remains deliberate.
* `--audio-source` accepts bare PipeWire names (`ozma-vm1.monitor`),
  `object:<name>`, `monitor:<sink>`, `node:<id>`, and an explicit `default`.
  `--audio-backend` accepts `auto` (PipeWire then Pulse), `pipewire`, and
  `pulse` (`parec`). Only `auto` may use the other backend.
* argv is accepted verbatim from `helper_argv`: `--sock --shm --width --height
  --fps`, plus optional `--audio-source/--audio-backend`,
  `--stun-server`, and `--turn-server/--turn-user/--turn-password`.

## 2. Interoperability evidence

### 2.1 Host candidates, 640x480@30

Command shape:

```bash
./ozma-webrtc/target/debug/ozma-webrtc-native \
  --sock /tmp/opencode/native-webrtc/signal.sock \
  --shm /tmp/opencode/native-webrtc/frame.bgra \
  --width 640 --height 480 --fps 30

python3 scripts/webrtc/native-interop-client.py \
  --sock /tmp/opencode/native-webrtc/signal.sock \
  --shm /tmp/opencode/native-webrtc/frame.bgra \
  --width 640 --height 480 --fps 30 --duration 6 --min-frames 30 \
  --bitrate 2500000
```

Client summary (exact output, `framerate` is reported as `0/1` by GStreamer's
fakesink caps because the decoder does not set it):

```json
{
  "ok": true,
  "answer_type": "answer",
  "session": "070cd3b0",
  "offer_candidates": 9,
  "answer_candidates": 6,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "ice_gathering_state": "complete",
  "decoded_video": {
    "buffers": 155,
    "bytes": 71424000,
    "caps": "video/x-raw, format=(string)I420, width=(int)640, height=(int)480, interlace-mode=(string)progressive, pixel-aspect-ratio=(fraction)1/1, chroma-site=(string)mpeg2, colorimetry=(string)bt601, framerate=(fraction)0/1"
  },
  "decoded_dimensions": [640, 480],
  "trickle": {"ok": true, "remote_candidates": 1, "session": "070cd3b0"},
  "bitrate": {"ok": true, "bitrate": 2500000}
}
```

Answer media shape:

```text
m=video 9 UDP/TLS/RTP/SAVPF 96
a=setup:active
a=mid:video0
a=rtpmap:96 H264/90000
a=sendonly
a=candidate:... typ host        # 6 host candidates (UDP, v4/v6)
```

The answer reused the offered PT 96; the decoder counted 155 frames at the
expected 640x480. The helper log for the same session reported
`ice_candidates=3 ice_types=host=3 remote_candidates=9`.

**Reliability note.** One early release attempt (the first of four) reached ICE
`failed` after about 11 s despite host candidates; three consecutive reruns and
all debug runs connected and decoded. The helper now omits IPv6 **link-local**
(`fe80::/10`) candidates from the answer: `webrtc-rs` emitted a candidate for an
interface it refused to bind (`could not listen udp fe80...`), and a peer cannot
route a link-local address without the original scope id, so those pairs only
delay ICE. That is a mitigation, not a proven root cause for the single
failure; ICE should still be treated as retry-worthy.

### 2.2 STUN enabled

With `--stun-server stun.l.google.com:19302` the answer contained a real
server-reflexive candidate and media still decoded:

```text
a=candidate:1846809296 1 udp 1694498815 103.214.221.33 50170 typ srflx raddr 0.0.0.0 rport 50170
a=candidate:1846809296 2 udp 1694498815 103.214.221.33 50170 typ srflx raddr 0.0.0.0 rport 50170
```

```json
{
  "ok": true,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "answer_candidates": 8,
  "decoded_video": {"buffers": 180, "caps": "... width=(int)640, height=(int)480 ..."},
  "trickle": {"ok": true},
  "bitrate": {"ok": true, "bitrate": 2500000}
}
```

TURN was not live-verified (no TURN server or credentials on this host); URL
normalisation and the credential path are unit-tested, and `turn:` is passed to
`webrtc-rs` as an `RTCIceServer`. This is the one ICE path without live
evidence.

### 2.3 Opus audio on, with real packets

The source was a disposable null sink and a live 440 Hz tone, not a default
device:

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

Exact client result (exit 0):

```json
{
  "ok": true,
  "connection_state": "connected",
  "ice_connection_state": "completed",
  "audio_offered": true,
  "decoded_video": {
    "buffers": 216,
    "caps": "video/x-raw, format=(string)I420, width=(int)640, height=(int)480, ..."
  },
  "decoded_audio": {
    "packets": 406,
    "packet_bytes": 120493,
    "buffers": 406,
    "bytes": 1559040,
    "caps": "audio/x-raw, format=(string)S16LE, ... rate=(int)48000, channels=(int)2 ..."
  },
  "trickle": {"ok": true},
  "bitrate": {"ok": true, "bitrate": 2500000}
}
```

The answer reused the offered Opus PT 111 and was `sendonly`:

```text
a=group:BUNDLE video0 audio1
m=video 9 UDP/TLS/RTP/SAVPF 96
a=mid:video0
a=rtpmap:96 H264/90000
a=sendonly
m=audio 9 UDP/TLS/RTP/SAVPF 111
a=mid:audio1
a=rtpmap:111 OPUS/48000
a=sendonly
```

The helper's final counters for that run:

```text
capture_frames=4775 capture_dropped=0 frames_assembled=5093
frames_dropped_idle=4062 packets_encoded=1031 encoded_bytes=307571
packets_written=1031 packets_dropped_full=0 encode_errors=0
write_errors=0 source_errors=0 pending_dropped_samples=0
```

That is 1031 packets for the sessions that were actively streaming audio; the
client above decoded 406 during its 8 s window, while another short session
decoded 204. The packet rate is ~50/s (20 ms cadence). No audio packet was
dropped.

The same source through the Pulse backend (`--audio-backend pulse`, `parec`)
decoded **204 Opus packets into 204 PCM buffers** and **60 H.264 frames** at
320x240@15, with the client exiting 0.

### 2.4 Audio off or unavailable

Without `--audio-source`, the same client offering Opus decoded **0 audio
packets / 0 PCM buffers** while **45 H.264 frames** arrived at 320x240@15; the
answer stayed `inactive`.

A deliberately misspelled PipeWire target produced:

```text
WARN could not start native audio; continuing video-only with an inactive audio m-line
     error=audio source `definitely-not-a-real-source` is not present in the
     PipeWire graph; refusing to fall back to the default source
```

The answer was `inactive`, **0 audio packets** decoded, and **30 H.264 frames**
still arrived. No default microphone was opened.

### 2.5 What was not claimed

This run used GStreamer 1.28.9, not Chromium/Firefox/Safari. The previous
GStreamer and browser reports remain the browser evidence; this report adds only
native-helper + GStreamer evidence. No browser matrix was rerun.

## 3. Performance

Measured with the real GStreamer peer decoding and `/proc/<pid>/stat` sampling
over 4 seconds of steady streaming, using the **release** binary. "CPU" is
percent of one core for the whole helper process (BGRA conversion + OpenH264 +
WebRTC/SRTP). RSS is resident memory while streaming.

| Case | CPU | RSS | Encoded samples | Decoded frames | Queue drops |
|---|---:|---:|---:|---:|---:|
| 640x480 @ 30 | 8.5% | 24.6 MB | 306 / 306 | 276 | 0 |
| 1280x800 @ 30 | 22.0% | 47.7 MB | 305 / 305 | 275 | 0 |
| 1280x800 @ 15 | 13.2% | 43.5 MB | 153 / 153 | 138 | 0 |

Idle CPU with no peer is 0.0% because the reader skips and the encoder thread
waits on its queue; idle RSS in earlier runs was ~15.8 MB.

The decoded count is a little below the encoded count because the peer's
media window starts after ICE/DTLS and the 10 s report window includes that
startup; the helper itself dropped no frames in any release row.

For comparison, an unoptimized **debug** build measured 53.8% CPU at
640x480@30 and 88.8% at 1280x800@15; at 1280x800@30 the debug build saturated
one core and the bounded queue dropped 126 of 306 frames. Release is the
relevant build and keeps up comfortably on this host. A product path that also
wants 1280x800@60 or multiple simultaneous consoles still needs a hardware
encoder (NVENC/VAAPI) or an ffmpeg sidecar; the current single OpenH264 thread
is the ceiling.

## 4. Tests run

```text
cd ozma-webrtc
cargo check
  -> clean

cargo test
  -> 45 unit tests passed
  -> tests/helper_lifecycle.rs: 1 passed
```

Unit coverage includes:

* SDP split/append, candidate placement per m-line, `a=end-of-candidates`
  ordering, idempotence, offered H.264/Opus payload-type extraction, and the
  audio answer plan (`sendonly` with a source, `inactive` without);
* signaling parse/error shapes for offer, `add-ice-candidate`/`candidate`, and
  bitrate; control-character and `typ` validation; numeric-string indexes;
* 500 kbps–50 Mbps clamping and ack shape;
* STUN/TURN URL normalisation and rejection of credentials/whitespace/bad ports;
* exact BGRA frame-length validation, async wrong-size rejection;
* real OpenH264 encode with SPS/PPS/IDR for a first frame, P frame on a stable
  second frame, and a fresh IDR after a bitrate rebuild;
* audio target parsing (bare/object/monitor/node/default, rejection of empty
  and control-character names) and Pulse prefix stripping;
* 44.1 kHz mono -> 48 kHz stereo resampling with the expected 4800-frame
  result, mono duplication, and rejection of unsupported rates/channels;
* frame assembly into exact 960-sample 20 ms packets, PTS cadence, partial-frame
  flush, and the bounded pending-sample drop counter;
* real libopus encode of three 20 ms frames, each decoded back to 960 samples
  with a non-silent buffer, plus rejection of a wrong-sized frame;
* bounded packet queue (capacity 1: second packet counted as dropped);
* an audio lifecycle test that starts capture/writer tasks, records counters,
  and shuts them down;
* a real process test that creates the socket, returns error/bitrate/candidate
  acks, exits on SIGTERM, and leaves no socket behind.

The live interop run is a direct script (not part of `cargo test`) because it
needs GStreamer `gi` and a working `webrtcbin`.

## 5. Switching `ozma-node` to the native helper

The prototype originally required pointing the generic helper flag at the
binary. This checkout now integrates it explicitly (see
`docs/phase4-native-webrtc-integration-report.md`):

```bash
cargo build --release --manifest-path ozma-webrtc/Cargo.toml
./target/release/ozma-node \
  ... \
  --webrtc-native \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 15
```

* `--webrtc-native` / `OZMA_WEBRTC_NATIVE=true` selects the native backend.
  Without an explicit `--webrtc-helper` it looks next to the node binary and
  then on `PATH`; it is never substituted silently for a configured Python
  helper.
* `--webrtc-helper ozma-webrtc-native` also selects the native backend.
* Do **not** pass `--webrtc-helper-script`; the node drops it for native and
  spawns the binary with the same `--sock/--shm/--width/--height/--fps` argv.
* `--webrtc-audio-source`/`--webrtc-audio-backend` are forwarded unchanged and
  now produce real native Opus when the helper can open the source. The node's
  status contract still classifies the native backend as video-only, so
  `/api/v1/connection` continues to report `webrtc_audio_configured: false`
  even while audio flows; see gap 1 below.
* STUN/TURN flags are forwarded unchanged and are accepted by the native binary.
* `--multi-console-webrtc` runs one native helper per derived socket/shm; the
  per-console status now carries `backend: "native"`.
* The node reports `running` once the socket is connected. The native helper
  logs its own frame counters to stderr.

The node's stop path drops the signaling socket and then `kill_on_drop`s the
child; the native helper also supports SIGTERM/SIGINT and removes its socket and
pipeline tasks. The process test exercises that path directly.

## 6. Remaining gaps

1. **Node status still says native video-only.** The helper streams Opus when
   `--audio-source` is supplied, but `ozma-node`'s `WebRtcBackend` still marks
   the native backend video-only, so `webrtc_audio_configured` is `false`.
   Updating that status contract and its tests is a separate integration
   change; the helper itself is honest in its logs and SDP.
2. **TURN is not live-verified.** No TURN server was available; only STUN and
   host candidates were exercised. URL/credential handling is unit-tested.
3. **No browser matrix.** Chromium/Firefox/Safari were not rerun against the
   native helper. PT reuse and the Opus `sendonly`/`inactive` shapes are
   designed for those peers, but the browser evidence remains the earlier
   helper reports.
4. **Static SDP profile-level-id.** The answer says
   `profile-level-id=42001f;packetization-mode=1` for every size, while a large
   OpenH264 encode may choose a different level in its SPS. GStreamer tolerated
   it; a strict browser/level check could need SPS-derived fmtp.
5. **Single video track per helper.** No simulcast, no RTX/retransmission
   configuration beyond the default interceptors, no RTCP stats assertion.
   The bitrate ack proves the video encoder was reconfigured, not the measured
   send rate. Audio has one fixed 128 kbps setting and no signaling control.
6. **Session cleanup is socket/PC-state driven.** A peer that keeps the
   signaling socket open and never closes the PC relies on WebRTC state
   transitions; there is no idle timeout.
7. **Dependency isolation.** `webrtc` 0.11 is pinned in a separate lockfile
   because of the `x25519-dalek` conflict. Moving to a newer `webrtc`/`rtc`
   API may allow the crate to join the main workspace once that pin changes.
8. **Product integration is now opt-in.** `ozma-node` gained `--webrtc-native`
   and `/api/v1/connection.webrtc_backend`; it remains non-default. Nothing was
   committed. See `docs/phase4-native-webrtc-integration-report.md` for the
   integration test and live evidence.
9. **Audio has no receive path.** The helper sends Opus only (`sendonly`) and
   never decodes remote audio. The common repo and browser offers
   (`recvonly`/`sendrecv`) negotiate `sendonly`; a peer that insists on the
   helper receiving audio is not supported.

## 7. Cleanup

* The video-only and audio helpers received SIGTERM, logged their final
  counters, exited, and removed their Unix sockets.
* The disposable `module-null-sink` and its GStreamer tone were removed; the
  default sink and source were never changed.
* The GStreamer client and feeder threads exited after each run.
* `/tmp/opencode/native-audio` (logs, SDPs, reports, BGRA files) is removed
  after this report; no GStreamer, tone, `parec`, or helper processes were left
  running.
* No package was installed; `cargo` used only crates already fetched for this
  prototype.
