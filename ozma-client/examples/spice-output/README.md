# Outbound SPICE scene bridge

`spice-output.c` is a small C bridge from Ozma's composed/scene stream to
libspice-server's QXL display, playback, and authentication interfaces. It is
view-only. It does not implement input, cursor, clipboard, migration, or
SPICE video codec streams.

The bridge has two input paths:

* **OZFR** — the existing composed RGBA frame stream from `scene_rgba`. The
  bridge diffs successive frames and sends only damaged rectangles, with a
  full-frame fallback for large changes.
* **OZSC** — a structured scene stream. Image resources become QXL secondary
  surfaces; placements become `draw-copy` commands whose source is
  `SPICE_IMAGE_TYPE_SURFACE`. Moves do not resend pixels. Clears, image
  destruction, rectangular damage, and full-frame fallback are explicit
  records.

Both paths use TLS and a viewer ticket. The server binds loopback by default.

## Build

```sh
cc -Wall -Wextra -O2 ozma-client/examples/spice-output/spice-output.c \
   $(pkg-config --cflags --libs spice-server glib-2.0) \
   -o /tmp/ozma-spice-output
```

The example has no Rust dependencies and does not need to be built through
Cargo. The existing documented compile command still works.

## TLS and viewer authentication

There is no plaintext port and no `spice_server_set_noauth` path. Create a
local certificate and ticket:

```sh
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout key.pem -out cert.pem -days 2 \
  -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1'
printf 'local-dev-ticket\n' > pw.txt
```

Run the bridge (TLS on 5930, loopback only):

```sh
python3 ozma-client/examples/spice-output/make_test_stream.py --format ozsc --audio | \
  /tmp/ozma-spice-output \
    --port 5930 \
    --cert cert.pem --key key.pem \
    --password-file pw.txt
```

`--ca` defaults to `--cert`, which is correct for the self-signed development
certificate. For a real deployment, pass the issuing CA with `--ca`.

If `--password-file` is omitted, the bridge generates a random ticket and
prints it on stderr. `--listen` defaults to `127.0.0.1`; overriding it is
explicit.

### Viewer commands

`spicy` can use the certificate directly:

```sh
spicy --spice-ca-file="$PWD/cert.pem" \
      -h 127.0.0.1 -s 5930 -w local-dev-ticket
```

`spicy-screenshot` in the installed spice-gtk build does not expose a CA-file
option. Point OpenSSL's default trust file at the development certificate:

```sh
SSL_CERT_FILE="$PWD/cert.pem" \
  spicy-screenshot -h 127.0.0.1 -s 5930 -w local-dev-ticket \
  -o screenshot.ppm
```

Some spice-gtk builds also load `$HOME/.spicec/spice_truststore.pem`; the
`SSL_CERT_FILE` form worked in the test environment.

For a headless `spicy` window under Xvfb, force the software X11 backend;
otherwise the GTK window may not map in a machine without a GL device:

```sh
Xvfb :99 -screen 0 1024x768x24 &
LIBGL_ALWAYS_SOFTWARE=1 GDK_BACKEND=x11 DISPLAY=:99 \
  spicy --spice-ca-file="$PWD/cert.pem" \
        -h 127.0.0.1 -s 5930 -w local-dev-ticket &
# capture the "spice display" window
wid=$(DISPLAY=:99 xdotool search --onlyvisible --class spicy | head -1)
DISPLAY=:99 import -window "$wid" shot.png
```

### Rejection and viewer bound

* A wrong ticket is rejected by libspice-server with `Invalid password`. In
  the test client this appears as `SPICE_CHANNEL_ERROR_AUTH` (main-channel
  event 23) and a non-zero exit.
* libspice-server 0.16 permits **one authenticated viewer per server
  instance** unless the debug environment variable `SPICE_DEBUG_ALLOW_MC_ENV`
  is set. A second main-channel connection replaces the first. This is the
  bound the library can enforce; the bridge therefore accepts only
  `--max-viewers 1` and rejects larger values rather than pretending to
  enforce them.
* TLS certificate or key errors are fatal at startup. There is no
  unauthenticated compatibility mode.

## Input format

All integers are big-endian. The stream starts with a four-byte magic.

### OZFR (composed frames)

```text
"OZFR"
u32 width
u32 height
u32 stride            (must be width * 4)
u8  pixels[stride * height]   RGBA8
```

The first frame initializes the primary surface. Later frames of the same size
are compared with the last accepted frame. Contiguous changed row bands become
draw-copy damage rectangles; if at least 75% of pixels changed, or the damage
is too fragmented, the bridge sends one full-frame bitmap. The producer should
keep sending complete frames; it does not need to compute damage.

### OZSC (structured scene)

```text
"OZSC"
u32 version           (must be 1)
repeated records:
  u32 type
  u32 payload_length
  u8  payload[payload_length]
```

The first record must be `SETUP`.

| Type | Name | Payload | Effect |
| ---: | --- | --- | --- |
| 1 | `SETUP` | `u32 width, u32 height, u32 bg` (`0xRRGGBBAA`) | Fixed display size and default clear color. Must be first. |
| 2 | `FRAME_BEGIN` | `u32 frame_id` | Framing marker; no display command. |
| 3 | `IMAGE_DEF` | `u32 id, u32 w, u32 h, u32 stride, RGBA pixels` | Create/replace a QXL secondary surface and upload the image once. `stride` must be `w*4`. Resource ids are 32-bit; up to 63 live resources. |
| 4 | `IMAGE_FREE` | `u32 id` | Destroy the image's QXL surface. The producer should clear its last placement first if the pixels should disappear. |
| 5 | `PLACE` | `u32 id, i32 x, i32 y` | Draw the image surface onto the primary at `(x,y)`. No scaling, alpha blending, or source sub-rectangle. |
| 6 | `CLEAR` | `i32 x, i32 y, u32 w, u32 h, u32 color` | Fill a rectangle. `color == 0` uses the `SETUP` background. |
| 7 | `DAMAGE` | `i32 x, i32 y, u32 w, u32 h, u32 stride, RGBA pixels` | Bitmap draw-copy of a changed rectangle. This is the fallback for video, effects, or content not represented by an image surface. |
| 8 | `FULL_FRAME` | `u32 w, u32 h, u32 stride, RGBA pixels` | Full-frame bitmap fallback. Dimensions must match `SETUP`. |
| 9 | `FRAME_END` | `u32 frame_id` | Framing marker; no display command. |
| 10 | `AUDIO` | `u32 rate, u16 channels, u16 fmt, u32 frames, S16LE PCM` | Raw stereo playback PCM. `channels` must be 2 and `fmt` must be 1 (`S16`). |

Scene semantics:

* `PLACE` is an opaque paint. It does not implicitly erase the previous
  position. Send a `CLEAR`, `DAMAGE`, or `FULL_FRAME` for the old rectangle
  when moving an image.
* `IMAGE_DEF` replaces an existing resource with the same id: the old surface
  is destroyed and a new one is created.
* `IMAGE_FREE` destroys the surface. Placements that referenced it should
  already have been cleared.
* The bridge keeps a local composed snapshot. When a display client
  initializes, it receives one full-frame bitmap of that snapshot, so a viewer
  that connects late does not need the producer to replay history. Individual
  image surfaces are sent on demand by libspice-server when a later placement
  references them.
* Resource/image data is RGBA8 from the producer and is converted to the
  little-endian BGRX layout used by `SPICE_SURFACE_FMT_32_xRGB`.

A ready-to-run example producer is `make_test_stream.py`:

```sh
python3 make_test_stream.py --format ozsc --audio --loop
python3 make_test_stream.py --format ozfr --loop
```

## Display update model

| Update | Wire representation |
| --- | --- |
| Initial display | One full-frame `draw-copy` bitmap; primary surface also carries the initial contents. |
| OZFR small change | One or more `draw-copy` bitmap commands for changed rectangles. |
| OZFR large/fragmented change | One full-frame `draw-copy` bitmap. |
| OZSC image definition | `surface-create` plus one bitmap `draw-copy` into that surface. |
| OZSC image move | `draw-copy` with `SPICE_IMAGE_TYPE_SURFACE`; no pixel upload. |
| OZSC image removal | `surface-destroy` (plus any producer-supplied clear). |
| OZSC clear | QXL `draw-fill`. |
| OZSC damage/video rectangle | `draw-copy` bitmap for just that rectangle. |
| Late viewer | Full-frame bitmap from the bridge's composed snapshot. |

The bridge explicitly sets `SPICE_STREAM_VIDEO_OFF`. It does not claim SPICE
video-stream or codec support; video-like content uses the bitmap damage or
full-frame fallback.

`--stats` is not a separate option; the bridge prints counters at shutdown:

```text
[spice-output] display summary: surface_create=2 surface_destroy=1 draw_surface=4 \
  draw_bitmap=7 draw_fill=3 full_frames=4 damage_rects=1
```

`full_frames` includes viewer-resync frames.

## Audio

The bridge registers a `SPICE_INTERFACE_PLAYBACK` instance and accepts raw
S16LE stereo PCM at 44.1 kHz by default. The server advertises playback and may
encode the PCM to Opus or CELT for viewers that support it; the bridge itself
does not accept an Opus input stream.

When the viewer has no playback channel, PCM is buffered up to 2 MiB and then
the oldest half is dropped, so a disabled audio client cannot grow memory
without bound. `spicy-screenshot` does not open the playback channel and
therefore consumes no audio. `spicy` does; the test run sent 131 PCM frames
through the playback channel.

## Verification

Tested in this environment with libspice-server 0.16, GLib 2.90, spice-gtk
0.43, `spicy-screenshot`, Xvfb, and a self-signed certificate:

| Check | Result |
| --- | --- |
| Build with `pkg-config --cflags --libs spice-server glib-2.0` | Pass, no warnings with `-Wall -Wextra` |
| TLS + correct ticket, `spicy-screenshot` | Pass; `320x200` and `400x240` screenshots captured |
| TLS + wrong ticket | Pass; server logs `Invalid password`, client exits 1 with auth-channel event 23 |
| OZFR initial frame and successive damage updates | Pass; box moved across three viewers; old and new pixels verified with `damage_rects` counters |
| OZFR full-frame fallback | Pass; a complete 200x120 red-to-blue change produced `full_frames=4 damage_rects=0` |
| OZFR late viewer | Pass; each new viewer received the current composed frame |
| OZSC `FULL_FRAME` fallback | Pass; a full magenta frame replaced the background |
| OZSC separate image surfaces | Pass; two images appeared at independent positions |
| OZSC move without pixel re-upload | Pass; image moved and old rectangle cleared |
| OZSC `IMAGE_FREE` / `surface-destroy` | Pass; freed image disappeared and counters showed one destroy |
| OZSC `CLEAR`, `DAMAGE` | Pass; background restore and yellow damage rectangle verified |
| OZSC late viewer | Pass; full-frame resync showed the current scene |
| Single-viewer bound / replacement | Pass; second viewer caused the first main channel to disconnect |
| Audio playback channel | Pass at protocol level under Xvfb: playback channel initialized and 131 PCM frames were sent to `spicy` |
| `spicy` GUI screenshots under Xvfb | Pass with `LIBGL_ALWAYS_SOFTWARE=1 GDK_BACKEND=x11`; three captures of the "spice display" window showed the red image move, the green image disappear, and the yellow damage rectangle appear |

Not implemented or not verified:

* No Opus input or passthrough. Opus/CELT encoding is left to
  libspice-server; A/V sync and audible output were not verified.
* No scaling, source sub-rectangles, alpha blending, or rotated placements for
  image surfaces.
* No SPICE video codec/stream negotiation (explicitly disabled), multi-monitor,
  cursor, input, clipboard, file transfer, or migration.
* No SASL. Authentication is TLS plus a ticket/password.
* No per-IP rate limiting. The library-enforced viewer bound is one; a second
  viewer replaces the first rather than being refused at the application
  layer.
* At most 63 live image resources (surface ids 1–63) and 64 QXL surfaces
  including the primary.
* The bridge exits when stdin closes, so use `--loop` in the test producer
  while interacting with a viewer. It also waits for the first frame or
  `SETUP` before opening the TCP/TLS listener.
* Encrypted TLS private keys are not handled by this example; use an
  unencrypted key or terminate with a deployment-provided key manager.
