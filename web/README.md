# Ozma browser client demo

This is the runnable demo for the **native Rust client compiled to wasm32**.
It is not a JavaScript reimplementation of the protocol:

- `ozma-web` (Rust → `web/pkg/`) contains the `#[wasm_bindgen]` API, WebCodecs
  video/audio decode, browser camera/microphone encode, canvas presentation,
  and DOM input translation.
- `ozma-client-core` owns the session driver, channel framing, and video
  reassembly.
- `ozma-transport-web` owns WebTransport (HTTP/3) and certificate pinning.
- `web/main.js` only creates the session over the node's JSON API and forwards
  DOM events. No protocol bytes are handled in JavaScript.

## Prerequisites

```sh
rustup target add wasm32-unknown-unknown
```

Install `wasm-bindgen-cli` at the **exact** version pinned in `Cargo.lock`
(currently `0.2.118`):

```sh
cargo install wasm-bindgen-cli --version 0.2.118 --locked
wasm-bindgen --version
```

If the CLI version and the `wasm-bindgen` crate version differ, the generated
JS refuses to load the wasm module with a schema mismatch. `build.sh` checks
this for you.

## Build

```sh
./web/build.sh
```

It runs:

```sh
RUSTFLAGS="--cfg=web_sys_unstable_apis" \
  cargo build -p ozma-web --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir web/pkg \
  target/wasm32-unknown-unknown/release/ozma_web.wasm
```

`web_sys_unstable_apis` is required for the WebCodecs and WebTransport web-sys
bindings. It is set on the command (and by the script), not in a
workspace-wide `.cargo/config.toml`, because it would affect native crates.

## Run

1. Start a development node with the browser listener and the JSON API:

   ```sh
   OZMA_AUDIO_SOURCE=fake cargo run -p ozma-node -- \
     --http-port 7382 \
     --webtransport-port 7443 \
     --webtransport-bind 127.0.0.1 \
     --cap video,audio
   ```

   `--webtransport-port 0` (the default) disables the browser listener. The
   node advertises the WebTransport endpoint in each session offer as a
   `quic` transport with an `https://` endpoint, and serves the listener's
   certificate at the additive `GET /api/v1/webtransport` endpoint.
   `OZMA_AUDIO_SOURCE=fake` makes the node send a paced deterministic tone, so
   `stats().audioLevel` is non-zero without a local audio device.

2. Serve this directory:

   ```sh
   node web/serve.mjs
   # Ozma web demo: http://127.0.0.1:8080/
   ```

   `serve.mjs` is a dependency-free static server. It also accepts
   `--port 9000 --host 127.0.0.1`.

3. Open `http://127.0.0.1:8080/` in a Chromium-based browser or Firefox, keep
   the node API URL at `http://127.0.0.1:7382`, and press **Connect**.

The demo creates a session, starts it, reads the WebTransport certificate,
connects `OzmaClient`, and forwards canvas pointer/wheel/key events. The status
line and stats panel show session state, decoded frames, drops, decode errors,
canvas size, bytes received, and the camera/microphone uplink counters.

## Camera and microphone uplink

The page has **Share camera**, **Share microphone**, **Start capture**, and
**Stop capture** controls. Both tracks are enabled by default; deselect one to
capture only the other. The session spec sets `audio.mic = true` so the node
grants the Mic permission and the `mic` channel is negotiated. The camera
channel is always part of the default channel map.

**Start capture** is deliberately called from the click handler: browsers only
allow `getUserMedia` and `AudioContext.resume()` during a user gesture. The
browser prompts for camera and microphone permission the first time. Both
`http://127.0.0.1` and `http://localhost` are secure contexts, so no certificate
is needed for the demo page itself.

- **Camera**: `getUserMedia({video: {width: {ideal: 1280}, height: {ideal:
  720}, frameRate: {ideal: 30}}})` into a hidden `<video>`. A 30 Hz timer pulls
  `VideoFrame`s into a WebCodecs `VideoEncoder`. The encoder probes
  `avc1.42E01E`, `avc1.4D401F`, and `avc1.640028`, first with
  `hardwareAcceleration: "prefer-hardware"` and then `"no-preference"`, sets
  `avc: {format: "annexb"}` (required by the node's H.264 decoder),
  `latencyMode: "realtime"`, 2.5 Mbps, and 30 fps. Access units are split into
  ≤1100-byte fragments with `VideoFragmentHeader` and sent on the `camera`
  datagram channel. The probe returns the exact codec and hardware mode that
  succeeded, and `configure` reuses both. Above two queued frames the newest
  frame is dropped and `cameraDropped` is incremented.
- **Microphone**: `getUserMedia({audio: {channelCount: {ideal: 2}, sampleRate:
  {ideal: 48000}, echoCancellation: true, noiseSuppression: true,
  autoGainControl: true}})`. A 48 kHz `AudioContext` feeds a
  `MediaStreamAudioSourceNode` forced to `channelCount = 2` and
  `channelCountMode = "explicit"`, then a 4096-sample `ScriptProcessorNode`
  (2 in / 2 out). The processor's input is accumulated into 960-sample
  (20 ms) f32-planar stereo blocks and encoded with WebCodecs Opus at 64 kbps.
  One raw Opus packet goes out per `mic` datagram. The processor's output is
  silent, so capture does not echo into the speakers.
- **State**: start/stop/error send `ControlMessage::CaptureStart`,
  `CaptureStop`, and `CaptureState` with the real codec and dimensions. A
  `KeyframeRequest` from the node forces the next camera frame to be an IDR.
  `request_keyframe()` sends `ControlMessage::KeyframeRequest` to the node.
- **Errors**: a missing codec returns `capture.unsupported`; a denied
  permission returns `capture.permission`; a missing or busy device returns
  `capture.device`. The error is displayed under the capture controls and is
  also reported to the node in `CaptureState.error`. If one track fails the
  other keeps running.
- **Preview**: tick **Preview camera** to reveal the muted local `<video>`.
  It is hidden by default.

## Capture verification API

The demo exposes two globals for browser automation (the page itself does not
use them):

```js
window.ozmaClient.stats();          // presentation + capture counters
window.ozmaClient.capture_state();  // {video, audio, videoCodec, width, height, error, errorCode}
window.ozmaSession;                 // {id, node} for /api/v1/sessions/{id}/capture
```

`capture_state()` example after a successful start:

```json
{
  "video": true,
  "audio": true,
  "videoCodec": "avc1.4D401F",
  "width": 1280,
  "height": 720,
  "error": null,
  "errorCode": null
}
```

## Certificate pinning

The browser cannot use a self-signed node certificate through the normal CA
path, so the client pins it:

- `main.js` fetches `GET /api/v1/webtransport` and uses its
  `certificate_der_base64`. If the endpoint is absent (older node), it falls
  back to `GET /api/v1/transport`.
- `OzmaClient.connect` base64-decodes the DER, computes SHA-256, checks it
  against the offer's `certificate_fingerprint` when present, and passes the
  32-byte hash as `BrowserConnectOptions.certificate_sha256`.
- The browser's `serverCertificateHashes` requires an **ECDSA** certificate
  with a validity period of at most 14 days. The node's generated WebTransport
  certificate follows those constraints; a browser certificate error is
  surfaced as `client.transport` / `client.certificate` in the status line.

The node API must be reachable from the page's origin. The development node
serves permissive CORS, so `http://127.0.0.1:8080` can call
`http://127.0.0.1:7382`.

## Secure context notes

- WebTransport and WebCodecs require a secure context. `http://localhost` and
  `http://127.0.0.1` are treated as secure contexts, which is why the demo is
  served over plain HTTP on loopback.
- To open the demo from another machine, serve it over HTTPS (or use the
  browser's insecure-origin development flags), and bind the WebTransport
  listener to a reachable address with `--webtransport-bind 0.0.0.0`.
- The page and the WebTransport endpoint may have different origins; the
  session offer's `https://` URL is used directly.

## Audio

Opus is decoded at 48 kHz stereo. Browsers start an `AudioContext` suspended
until a user gesture; the demo calls `resume_audio()` from the first canvas
pointer press. Decoded buffers are scheduled with a ~60 ms lead and dropped if
they fall more than 250 ms behind the audio clock.

An `AnalyserNode` sits between the scheduled buffers and the destination. It
does not alter the samples; `stats().audioLevel` is the RMS of the most recent
1024-sample time-domain window (0..1). `stats().avDriftMs` subtracts the
scheduled audio playout position from the timestamp of the most recently
presented video frame, both on the `performance.now()` clock. Negative values
mean video was presented before the audio it belongs to. This is a
measurement only: the client does not resample or delay video to claim
lip-sync.

## Input mapping

`ozma-web` exports `jsCodeToHid(code)`, which maps `KeyboardEvent.code` to a USB
HID usage id (page 0x07). The table covers letters, digits, arrows, modifiers,
Enter, Escape, Tab, Backspace, Space, F1–F12, punctuation/editing keys, and the
numeric keypad. `send_key` takes that HID usage id, never a JS `keyCode`.
Pointer buttons use the DOM `MouseEvent.button` index (0 left, 1 middle, 2
right, 3 back, 4 forward).

Input is sent immediately from the DOM handler through the transport's
cloneable `BrowserSender`, not through a queue drained by the media loop.
Reliable frames are encoded and enqueued synchronously, so two events can never
reorder and an idle transport does not delay input until the next media event.
`close()` sends `ReleaseAll`, then `Bye`, then `BrowserSender::close`, which
wakes a pending `recv` immediately.

## Verification

Host tests for the pure input and certificate-pinning helpers:

```sh
cargo test -p ozma-web
```

Wasm type check (the command `build.sh` uses first):

```sh
RUSTFLAGS="--cfg=web_sys_unstable_apis" \
  cargo check -p ozma-web --target wasm32-unknown-unknown
```

The wasm check and bundle build require the `ozma-client-core` and
`ozma-transport-web` crates in this workspace to compile. If another
workstream is mid-edit there, run the check again after it lands.

### Browser results (Playwright, fake media devices)

Tested against a local node started with:

```sh
OZMA_AUDIO_SOURCE=fake target/debug/ozma-node --name wasm-capture-test \
  --http-port 7382 --webtransport-port 7443 --cap video,audio
node web/serve.mjs --port 8080
```

Chromium (`--use-fake-device-for-media-stream
--use-fake-ui-for-media-stream --no-sandbox`), representative run after ~4 s
of capture:

```text
cameraFrames 118  cameraBytes 214872  cameraDropped 0  cameraErrors 0
cameraWidth 1280  cameraHeight 720  videoCodec avc1.4D401F
micPackets 196  micBytes 27290  micErrors 0
audioLevel 0.176  avDriftMs -80.5
node: video.frames 117, video.width 1280, video.height 720,
      video.decode_errors 0, audio.packets 196, audio.samples 376320
PPM: status 200, "P6\n1280 720\n255\n"
```

Firefox (`media.navigator.streams.fake=true`,
`media.navigator.permission.disabled=true`), same flow:

```text
cameraFrames 119  cameraBytes 318581  cameraDropped 0  cameraErrors 0
cameraWidth 1280  cameraHeight 720  videoCodec avc1.42E01E
micPackets 196  micBytes 52720  micErrors 0
audioLevel 0.171  avDriftMs -310.0
node: video.frames 78, video.width 1280, video.height 720,
      video.decode_errors 0, audio.packets 196, audio.samples 376320
PPM: status 200, "P6\n1280 720\n255\n"
```

The exact frame counts, byte totals, levels, and drift vary between runs. The
Firefox node figure is lower than the client's encoded frame count because the
camera channel is unreliable: losing any one fragment discards that access
unit during reassembly. Decode errors stayed at zero.

Firefox honoured `avc: {format: "annexb"}` in the tested build, so no AVCC
framing failure occurred. The camera pipeline still compares the first encoded
chunk against Annex-B start codes and reports `capture.unsupported` instead of
sending AVCC if a future browser ignores the option.

Idle-send/close path (same runs): a pointer event sent after 2 s without input
returned in ≤0.2 ms, and `close()` while the media loop was otherwise waiting
reported `closed` in 6 ms (Chromium) / 5 ms (Firefox); the listener cleared
`window.ozmaClient`. The transport still receives the node's downstream video
during this test, so the loop is not literally event-free; the point is that
input and close no longer depend on a frame arriving. `request_keyframe()`
returned without error and the node logged `Forcing the next encoded frame to
be a keyframe`.

The fake devices in these browser runs honoured the requested 1280×720 camera
size rather than a 640×480 default. The node decoded that size with zero
errors. A 640×480 device is also accepted; the pipeline always configures the
encoder from the live `<video>` dimensions and forwards the real size in
`CaptureState`.

## What is not verified here

- **A/V lip-sync.** `avDriftMs` measures the difference between the scheduled
  audio playout position and the most recently presented canvas frame. It is
  not a correction and does not account for display scanout, audio device
  latency, or encoder timestamps. Expect a stable offset near `-LEAD_SECONDS`
  when both streams are healthy, not a guarantee.
- **Hardware codec coverage.** The encoder probes H.264 profiles and both
  hardware preferences, but which combination is supported varies by browser,
  OS, GPU, and headless mode. Headless Chromium on this host supported Main and
  High but not Baseline at 1280×720, and rejected `"prefer-hardware"`; the
  probe correctly fell back to `"no-preference"`.
- **USB/UAC2 injection.** The node stores and counts decoded browser camera
  frames and microphone PCM. Routing decoded microphone audio into the node's
  UAC2/ALSA target or exposing the camera as a UVC device is not implemented in
  this workstream.
- **`requestVideoFrameCallback`.** web-sys 0.3.95 does not expose it, so the
  camera uses a 30 Hz `setTimeout` loop. A high-refresh camera still produces at
  most 30 captured frames per second.
- **ScriptProcessorNode.** The microphone uses a `ScriptProcessorNode` (4096
  samples) as specified. Browsers log a deprecation warning; an `AudioWorklet`
  would avoid it but adds a separate module-loading path.
- **Scene overlays and HDR** are not presented by this demo; it draws decoded
  H.264 frames only.
