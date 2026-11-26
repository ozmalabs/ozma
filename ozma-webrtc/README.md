# ozma-webrtc-native (prototype)

A native Rust replacement for the GStreamer/Python WebRTC helper. It speaks the
same Unix-socket signaling protocol and reads the same packed-BGRA shared-memory
frame file, but uses `webrtc-rs` for ICE/DTLS/SRTP/RTP, OpenH264 for a real
H.264 track, and libopus for an optional real Opus track.

This crate is deliberately **not** a member of the main workspace: `webrtc-dtls`
pins stable `x25519-dalek ^2`, while `ozma-crypto` in the main lockfile pins
`=2.0.0-rc.3`, and Cargo cannot unify them. It has its own `Cargo.lock` so it
cannot destabilise `ozma-node`'s dependency graph.

## Build and test

```bash
cd ozma-webrtc
cargo build --release
cargo test
```

## Run

```bash
./target/release/ozma-webrtc-native \
  --sock /tmp/ozma-native.sock \
  --shm /tmp/ozma-native.bgra \
  --width 1280 --height 800 --fps 15
```

The helper creates `--sock`, waits for a newline-delimited JSON `offer`, and
writes encoded H.264 from the BGRA file. It gathers host candidates by default;
add `--stun-server stun:host:port` and/or
`--turn-server turn:host:port --turn-user U --turn-password P`.

### Optional audio

Audio is opt-in. Without `--audio-source` the helper is video-only and an
offered Opus m-line is answered `inactive`. With a source:

```bash
./target/release/ozma-webrtc-native \
  ... \
  --audio-source ozma-vm1.monitor \
  --audio-backend pipewire
```

`--audio-source` values:

| Value | Meaning |
|---|---|
| `ozma-vm1.monitor` | Bare PipeWire object/monitor name (what the node sends) |
| `object:alsa_input.usb-mic` | Named PipeWire object |
| `monitor:ozma-vm1` | Monitor of a named sink (`ozma-vm1.monitor`) |
| `node:42` | Numeric PipeWire node id |
| `default` | Explicit default source; never selected implicitly |

`--audio-backend` values:

| Value | Behaviour |
|---|---|
| `auto` (default) | Native PipeWire capture, then `parec` fallback |
| `pipewire` | Native PipeWire capture only |
| `pulse` | `parec` (PulseAudio / PipeWire compatibility) only |

A named PipeWire target is checked against the graph before opening. An unknown
name is an error, not a silent fall back to the session default (which could be
a microphone). When the configured audio source cannot be opened, the helper
logs a warning and continues video-only; the answer stays `inactive`.

Captured mono/stereo PCM is converted to interleaved stereo and linearly
resampled to 48 kHz, assembled into exact 20 ms frames (960 samples/channel),
and encoded at a fixed **128 kbps** with libopus. Every hand-off is bounded:
8 capture frames in the PipeWire stream, at most four 20 ms frames in the
reframer, and 8 encoded packets before the RTP writer. A full queue drops the
newest unit and increments a counter. Signaling `bitrate` messages still change
video only.

An offered Opus m-line reuses the peer's payload type. With a live audio source
the answer is `sendonly`; without one it is `inactive`. Video keeps its existing
`sendonly` m-line.

## Switch `ozma-node` to it

Build the binary and select the native backend explicitly:

```bash
cargo build --release --manifest-path ozma-webrtc/Cargo.toml
./target/release/ozma-node \
  ... \
  --webrtc-native \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 15
```

`--webrtc-native` (or `OZMA_WEBRTC_NATIVE=true`) looks for
`ozma-webrtc-native` next to the node binary and then on `PATH`. An explicit
path also works:

```bash
./target/release/ozma-node ... \
  --webrtc-helper "$PWD/ozma-webrtc/target/release/ozma-webrtc-native" \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 15
```

Do not pass `--webrtc-helper-script`: the node drops it for the native backend.
The node's argv (`--sock`, `--shm`, `--width`, `--height`, `--fps`, optional
audio/STUN/TURN) is accepted as-is. Add `--webrtc-audio-source` to send audio.
The node's `/api/v1/connection` still classifies the native backend as
video-only, so its `webrtc_audio_configured` field reports `false` even while
the helper is streaming audio; this is a status integration gap, not a helper
limitation.

## Live interop check

`../scripts/webrtc/native-interop-client.py` is a real GStreamer `webrtcbin`
peer that also feeds an animated BGRA pattern. With `--audio` it offers and
decodes an Opus m-line and counts real Opus packets and decoded PCM buffers:

```bash
python3 scripts/webrtc/native-interop-client.py \
  --sock /tmp/ozma-native.sock --shm /tmp/ozma-native.bgra \
  --width 640 --height 480 --fps 30 --duration 6 \
  --min-frames 30 --audio --min-audio-frames 20
```

Exit 0 means ICE/DTLS connected, at least `--min-frames` real H.264 frames were
decoded at the expected dimensions, at least `--min-audio-frames` Opus packets
were decoded when requested, and the trickle/bitrate acks succeeded.
