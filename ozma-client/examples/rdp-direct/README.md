# Direct-surface RDP bridge

`rdp-direct.c` is a small FreeRDP 3 server helper that reads the same `OZFR`
stream as the original `rdp_output` prototype, but publishes frames directly
into FreeRDP's in-memory shadow surface. There is no Xvfb, no ffplay, and no
separate desktop window to capture.

The helper installs a custom FreeRDP shadow subsystem
(`shadow_subsystem_set_entry`) which owns the primary surface and peer
lifecycle. Each frame is converted from packed RGBA8 to the surface's BGRX32
layout, marked invalid, and published with `shadow_subsystem_frame_update`.
FreeRDP's shadow client then sends the changed region to connected viewers as
bitmap updates. GFX/RFX/NSC negotiation is deliberately disabled.

## What is direct and what is not

Direct:

- OZFR pixels go straight into the FreeRDP shadow server surface.
- No X server, compositor, ffplay, or BMP/raw-frame re-display in the path.
- FreeRDP handles TLS, peer handshake, and the on-the-wire bitmap updates.

Still composed or outside this bridge:

- The scene itself is composed to an `OZFR` frame before it reaches stdin
  (that is the existing `scene_rgba` output contract).
- RDPEGFX, AVC420/AVC444, and H.264 are not implemented here and are disabled.
- Input, audio, clipboard, dynamic resize, and reconnect-with-new-size are not
  implemented. Sessions are view-only.

## Build

```sh
cc -O2 -Wall -Wextra \
  ozma-client/examples/rdp-direct/rdp-direct.c \
  $(pkg-config --cflags --libs freerdp-shadow3) \
  -o /tmp/ozma-rdp-direct
```

On this host the installed packages are FreeRDP 3.33.1
(`freerdp-shadow-cli3`, `xfreerdp3`).

## Run

Generate a token (default: `/tmp/ozma-rdp-viewer-token`, mode 0600):

```sh
cargo run -p ozma-client --example scene_rgba -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene | \
  /tmp/ozma-rdp-direct --port 3390 --max-viewers 1
```

Or use a pre-provisioned credential file (`user:secret` per line, or one bare
token which is paired with user `ozma`):

```sh
printf '%s\n' 'ozma:correct-horse' > /tmp/ozma-rdp-creds
chmod 600 /tmp/ozma-rdp-creds

cargo run -p ozma-client --example scene_rgba -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene | \
  /tmp/ozma-rdp-direct --port 3390 --auth-file /tmp/ozma-rdp-creds
```

The Rust example can forward the stream and spawn the helper for you:

```sh
cargo run -p ozma-client --example scene_rgba -- \
  --node http://127.0.0.1:7382 --offer offer.json --scene | \
  cargo run -p ozma-client --example rdp_output -- \
    --direct-surface /tmp/ozma-rdp-direct --port 3390 --max-viewers 1
```

Viewer (stock FreeRDP):

```sh
xfreerdp3 /v:127.0.0.1:3390 /u:ozma /p:'<token>' \
  /sec:tls /cert:ignore /size:1280x720 -gfx -rfx /bpp:32
```

The bridge prints the exact viewer command on startup. The certificate is
self-signed and created on first use under `--config-dir` (or the FreeRDP
config directory), so `/cert:ignore` is expected for this development path.
FreeRDP 3.33.1 happens to log a spurious
`Certificate PEM ... valid, but some properties failed to be extracted` error
during successful initialization; TLS negotiation still succeeds (verified with
the stock viewer).

A viewer that requests audio capture may cause FreeRDP to log an `audin`
channel error. Audio is outside this bridge; the display connection is
unaffected.

## Authentication and policy

- TLS is always required. NLA, EXT, legacy RDP security, and RDP security layer
  are disabled, so the bridge's `Authenticate` callback receives the viewer
  username and token.
- A credential file or generated token is mandatory. Tokens are 32 random
  bytes rendered as 64 hex characters. Generated token files are created with
  mode 0600 and written atomically.
- Comparison is constant-time and secrets are never logged.
- The bind address defaults to `127.0.0.1`. A non-loopback bind prints a
  warning and still requires TLS and credentials.
- `--max-viewers` (default 1) is enforced before a viewer is activated. The
  rejected viewer receives RDP `ERRINFO_SERVER_INSUFFICIENT_PRIVILEGES`, and
  the bridge logs a specific reason.
- Sessions are read-only: `mayInteract` is false and every input callback is an
  intentional no-op.
- Logged rejections include missing credentials, unknown user/token, invalid
  token, and viewer-limit reached, with the peer hostname.

Example diagnostics:

```text
rdp-direct: direct surface 1280x720 on rdp://127.0.0.1:3390 (TLS required, view-only, max 1 viewers)
rdp-direct: viewer connected from 127.0.0.1 (view-only)
rdp-direct: viewer rejected from 127.0.0.1: invalid username or viewer token
rdp-direct: viewer rejected from 127.0.0.1: viewer limit reached (max 1 concurrent connection)
```

## Verification

`smoke-test.sh` builds the helper, starts an isolated Xvfb, feeds synthetic
OZFR frames, connects stock `xfreerdp3`, checks two screenshots differ, and
checks both a wrong-token rejection and the concurrent-viewer limit.

```sh
ozma-client/examples/rdp-direct/smoke-test.sh
```

It requires `cc`, `pkg-config`, FreeRDP shadow, `Xvfb`, `xfreerdp3`,
ImageMagick `import`/`compare`, and `python3`.
