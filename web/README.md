# Ozma browser client demo

This is the runnable demo for the **native Rust client compiled to wasm32**.
It is not a JavaScript reimplementation of the protocol:

- `ozma-web` (Rust → `web/pkg/`) contains the `#[wasm_bindgen]` API, WebCodecs
  video/audio decode, canvas presentation, and DOM input translation.
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
   cargo run -p ozma-node -- \
     --http-port 7382 \
     --webtransport-port 7443 \
     --webtransport-bind 127.0.0.1
   ```

   `--webtransport-port 0` (the default) disables the browser listener. The
   node advertises the WebTransport endpoint in each session offer as a
   `quic` transport with an `https://` endpoint, and serves the listener's
   certificate at the additive `GET /api/v1/webtransport` endpoint.

2. Serve this directory:

   ```sh
   node web/serve.mjs
   # Ozma web demo: http://127.0.0.1:8080/
   ```

   `serve.mjs` is a dependency-free static server. It also accepts
   `--port 9000 --host 127.0.0.1`.

3. Open `http://127.0.0.1:8080/` in a Chromium-based browser, keep the node API
   URL at `http://127.0.0.1:7382`, and press **Connect**.

The demo creates a session, starts it, reads the WebTransport certificate,
connects `OzmaClient`, and forwards canvas pointer/wheel/key events. The status
line and stats panel show session state, decoded frames, drops, decode errors,
canvas size, and bytes received.

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

## Input mapping

`ozma-web` exports `jsCodeToHid(code)`, which maps `KeyboardEvent.code` to a USB
HID usage id (page 0x07). The table covers letters, digits, arrows, modifiers,
Enter, Escape, Tab, Backspace, Space, F1–F12, punctuation/editing keys, and the
numeric keypad. `send_key` takes that HID usage id, never a JS `keyCode`.
Pointer buttons use the DOM `MouseEvent.button` index (0 left, 1 middle, 2
right, 3 back, 4 forward).

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

## What is not verified here

- **No browser automation on this host.** The wasm crate type-checks, the pure
  input and certificate helpers are unit-tested, and `web/build.sh` generated
  `web/pkg` with `wasm-bindgen` 0.2.118, but no script drove a real Chromium
  instance through WebTransport, WebCodecs, or Web Audio. Do not treat the demo
  as proof of end-to-end browser playback until someone runs it.
- **Codec support is probed, not guaranteed.** The client tries
  `avc1.42E01E`, `avc1.4D401F`, and `avc1.640028` with
  `hardwareAcceleration: "prefer-hardware"` then `"no-preference"`, and reports
  `client.unsupported` if none are supported. Browser/OS hardware decode
  coverage varies.
- **Keyframe recovery.** There is no client-to-node keyframe-request message in
  the protocol yet, so `OzmaClient.request_keyframe()` returns
  `client.unsupported`. After a decoder error the client drops until the node's
  next periodic keyframe.
- **Input wake-up.** Input is sent by the media loop before it waits for the
  next session event. A streaming session delivers input promptly; a completely
  idle transport can delay a queued event until the next control or media
  event. This avoids cancelling a pending transport read and losing reliable
  control bytes.
- **Scene overlays and HDR** are not presented by this demo; it draws decoded
  H.264 frames only.
