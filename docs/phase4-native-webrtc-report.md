# Phase 4: native Rust WebRTC helper report

**Scope:** replace the GStreamer/Python WebRTC helper with a standalone Rust
binary that speaks the node's existing signaling protocol and streams a real
OpenH264-encoded video track over `webrtc-rs`. This is a **prototype**, not the
product default: `ozma-node` is unchanged and still spawns whatever
`--webrtc-helper` names.

**Result: PASS for the prototype.** A real GStreamer `webrtcbin` peer completed
ICE and DTLS against the native helper and decoded **155 real H.264 frames at
640x480** (and 180 with STUN enabled, 149 with an offered Opus m-line). The
answer, trickle-candidate ack, and bitrate ack all match the node's contract.
No frames were claimed without a decoded I420 buffer count.

| Item | Value |
|---|---|
| Date | 2026-11-26 ~12:38–12:44 AEDT |
| Git HEAD | `1699cd1dc6c5058ea37541d157790bbb87f2fe7f` |
| Working tree | dirty with this prototype plus pre-existing concurrent edits; nothing committed by this work |
| CPU | AMD Ryzen 5 9600X (6C/12T) |
| Rust | 1.95.0 |
| GStreamer peer | 1.28.9 (`webrtcbin`, H.264, `avdec_h264`) |
| Encoder | OpenH264 0.9.10 (bundled, software, baseline profile) |
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
  src/shm.rs                 # exact-length BGRA validation/read
  src/sdp.rs                 # media-section split, candidate append, offered PT
  src/signaling.rs           # JSON messages, acks, bitrate clamp, ICE URLs
  src/server.rs              # webrtc-rs peer connections + signaling server
  tests/helper_lifecycle.rs  # real process: socket, acks, SIGTERM cleanup
scripts/webrtc/native-interop-client.py   # live GStreamer peer + BGRA feeder
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
```

* One encoder/track is shared by all peers; adding a peer does not add an
  encoder.
* Encoding is idle at 0% CPU while no peer is connected.
* A full queue drops the newest frame instead of buffering; counters are logged
  every 10 seconds and at shutdown.
* Keyframes: one IDR per second, on every new peer, and after a bitrate change.
  OpenH264 scene-change detection can add an IDR on a large visual change.

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
* An offered Opus m-line is answered **`inactive`** with a valid mid, rtpmap,
  and BUNDLE entry. Native audio capture is not implemented; rejecting the
  m-line without a mid made GStreamer reject the whole BUNDLE answer, so this
  compatibility path is deliberate.
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

### 2.3 Peer that offers Opus audio

The native helper advertises video only, but browser and GStreamer peers offer
an audio m-line. With an offered Opus m-line (PT 111, and separately PT 97) the
answer is valid and video still decodes:

```text
a=group:BUNDLE video0 audio1
m=video 9 UDP/TLS/RTP/SAVPF 96
a=sendonly
m=audio 9 UDP/TLS/RTP/SAVPF 111
a=mid:audio1
a=rtpmap:111 OPUS/48000
a=inactive
```

```text
PT 111: ok=true, decoded=149 I420 buffers, 640x480, trickle ok
PT  97: ok=true, decoded=120 I420 buffers, 640x480, trickle ok
```

### 2.4 What was not claimed

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
  -> 32 unit tests passed
  -> tests/helper_lifecycle.rs: 1 passed
```

Unit coverage includes:

* SDP split/append, candidate placement per m-line, `a=end-of-candidates`
  ordering, idempotence, offered H.264/Opus payload-type extraction;
* signaling parse/error shapes for offer, `add-ice-candidate`/`candidate`, and
  bitrate; control-character and `typ` validation; numeric-string indexes;
* 500 kbps–50 Mbps clamping and ack shape;
* STUN/TURN URL normalisation and rejection of credentials/whitespace/bad ports;
* exact BGRA frame-length validation, async wrong-size rejection;
* real OpenH264 encode with SPS/PPS/IDR for a first frame, P frame on a stable
  second frame, and a fresh IDR after a bitrate rebuild;
* a real process test that creates the socket, returns error/bitrate/candidate
  acks, exits on SIGTERM, and leaves no socket behind.

The live interop run is a direct script (not part of `cargo test`) because it
needs GStreamer `gi` and a working `webrtcbin`.

## 5. Switching `ozma-node` to the native helper

This is intentionally not wired into the product yet. To try it:

```bash
cargo build --release --manifest-path ozma-webrtc/Cargo.toml
./target/release/ozma-node \
  ... \
  --webrtc-helper "$PWD/ozma-webrtc/target/release/ozma-webrtc-native" \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 15
```

* Do **not** pass `--webrtc-helper-script`; the helper takes the flags directly.
* `--webrtc-audio-source`/`--webrtc-audio-backend` are accepted but produce
  video-only output (audio m-line answered `inactive`).
* STUN/TURN flags are forwarded unchanged and are accepted by the native binary.
* `--multi-console-webrtc` will also work at the process level (one helper per
  derived socket/shm), but it has not been exercised live with two native
  helpers.
* Status reporting is unchanged: the node reports `running` once the socket is
  connected. The native helper logs its own frame counters to stderr.

The node's stop path drops the signaling socket and then `kill_on_drop`s the
child; the native helper also supports SIGTERM/SIGINT and removes its socket and
pipeline tasks. The process test exercises that path directly.

## 6. Remaining gaps

1. **Audio is not native.** An offered **Opus** m-line is answered inactive with
   a valid mid/rtpmap/BUNDLE entry. A non-Opus audio m-line falls back to
   `webrtc-rs`'s bare rejected section (no mid), which a GStreamer BUNDLE peer
   may reject; every browser and both repo clients offer Opus. Replacing the
   Python worker's monitor audio still needs an Opus encoder and audio track.
2. **TURN is not live-verified.** No TURN server was available; only STUN and
   host candidates were exercised. URL/credential handling is unit-tested.
3. **No browser matrix.** Chromium/Firefox/Safari were not rerun against the
   native helper. PT reuse and the inactive-audio shape are designed for those
   peers, but the browser evidence remains the earlier helper reports.
4. **Static SDP profile-level-id.** The answer says
   `profile-level-id=42001f;packetization-mode=1` for every size, while a large
   OpenH264 encode may choose a different level in its SPS. GStreamer tolerated
   it; a strict browser/level check could need SPS-derived fmtp.
5. **Single video track per helper.** No simulcast, no RTX/retransmission
   configuration beyond the default interceptors, no RTCP stats assertion.
   The bitrate ack proves the encoder was reconfigured, not the measured send
   rate.
6. **Session cleanup is socket/PC-state driven.** A peer that keeps the
   signaling socket open and never closes the PC relies on WebRTC state
   transitions; there is no idle timeout.
7. **Dependency isolation.** `webrtc` 0.11 is pinned in a separate lockfile
   because of the `x25519-dalek` conflict. Moving to a newer `webrtc`/`rtc`
   API may allow the crate to join the main workspace once that pin changes.
8. **No product integration or commit.** No `ozma-node` source, parity matrix,
   or plan status was changed, and nothing was committed.

## 7. Cleanup

* Both helpers received SIGTERM and exited; their Unix sockets were removed.
* The GStreamer client and feeder threads exited after each run.
* `/tmp/opencode/native-webrtc` (logs, SDPs, reports, BGRA files) is removed
  after this report; no node, QEMU, GStreamer, or helper processes were left
  running.
* No package was installed; `cargo` used only crates already fetched for this
  prototype.
