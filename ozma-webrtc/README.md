# ozma-webrtc-native (prototype)

A native Rust replacement for the GStreamer/Python WebRTC helper. It speaks the
same Unix-socket signaling protocol and reads the same packed-BGRA shared-memory
frame file, but uses `webrtc-rs` for ICE/DTLS/SRTP/RTP and OpenH264 for a real
H.264 track.

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

`--audio-source` / `--audio-backend` are accepted for argv compatibility; native
audio is not implemented, and an offered Opus m-line is answered `inactive`.

## Switch `ozma-node` to it

Build the binary and point the existing helper flag at it without a script:

```bash
cargo build --release --manifest-path ozma-webrtc/Cargo.toml
./target/release/ozma-node \
  ... \
  --webrtc-helper "$PWD/ozma-webrtc/target/release/ozma-webrtc-native" \
  --webrtc-width 1280 --webrtc-height 800 --webrtc-fps 15
```

Do not pass `--webrtc-helper-script`. The node's argv (`--sock`, `--shm`,
`--width`, `--height`, `--fps`, optional audio/STUN/TURN) is accepted as-is.

## Live interop check

`../scripts/webrtc/native-interop-client.py` is a real GStreamer `webrtcbin`
peer that also feeds an animated BGRA pattern:

```bash
python3 scripts/webrtc/native-interop-client.py \
  --sock /tmp/ozma-native.sock --shm /tmp/ozma-native.bgra \
  --width 640 --height 480 --fps 30 --duration 6 --min-frames 30
```

Exit 0 means ICE/DTLS connected and at least `--min-frames` real H.264 frames
decoded at the expected dimensions.
