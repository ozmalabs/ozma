# Phase 4 VNC/RFB Server Report

Status: implemented and tested in `ozma-node`. The node can serve its own
opt-in RFB 3.8 endpoint for the controller's VNC consumers instead of only
registering an external QEMU VNC endpoint. Standard RFB VNC password
authentication (security type 2) is now available. The server offers **ZRLE**
(encoding 16), **Tight** (7), **Hextile** (5), and **ZLib** (6) when the
client advertises them, keeping **Raw** as the always-valid fallback, and
answers the **Cursor** pseudo-encoding with a typed empty cursor. Tight now
covers **JpegCompression** (honouring the -32..-23 quality pseudo-encoding),
the **GradientFilter**, and the -256..-247 compression-level hint in addition
to Fill/Palette/Copy. The server also implements **server-initiated
ExtendedDesktopSize** (-308): when the capture source changes geometry after
`ServerInit` (or on its first frame), each viewer that advertised -308 gets a
typed resize rectangle followed by the new framebuffer. A client
**`SetDesktopSize`** message is parsed and explicitly **denied**, because the
capture source has no resize API; the server never claims a resize it did not
perform. **The transport is still plain TCP: VNC authentication
is not encryption and there is no TLS. ZRLE, ZLib, and Tight are compression,
not encryption.** The node also has an **opt-in, bounded clipboard bridge**
(`--vnc-clipboard`) that uses the desktop-profile X11 clipboard; it is off by
default and never claims a sync when the backend is unavailable. No commit was
made.

## Scope

Added:

- `ozma-node/src/vnc_server.rs` — a bound RFB listener, handshake, optional
  RFB security type 2 (DES challenge/response), Raw, Hextile, ZLib, Tight, and
  ZRLE framebuffer encoders, Cursor pseudo-encoding, server-initiated
  ExtendedDesktopSize resize, typed `SetDesktopSize` denial, client-message
  parser, input adapter, opt-in bounded clipboard bridge, and per-listener
  counters.
- `ozma-node/Cargo.toml` — the `flate2` dependency for the per-client ZRLE,
  ZLib, and Tight zlib streams (already present in `Cargo.lock` through the
  image stack) and the `jpeg-encoder` dependency for Tight JPEG (the same
  crate `ozma-encode` already uses; no new crate was downloaded).
- `ozma-node/src/main.rs` — `--vnc-server`, `--vnc-bind`,
  `--vnc-server-port`, `--vnc-password-file`, `--vnc-allow-unauthenticated`,
  `--vnc-max-clients`, `--vnc-idle-timeout-seconds`, `--vnc-clipboard`, and
  `--vnc-clipboard-max-bytes`, startup/shutdown wiring, and endpoint logging.
- `ozma-node/src/config.rs` — validation and storage for the new flags,
  including refusal of unauthenticated non-loopback binds.
- `ozma-node/src/registration.rs` — served-endpoint precedence over an external
  QEMU endpoint, and documentation that the advertised endpoint may require a
  password.
- `ozma-node/src/platform.rs` — test-config field updates.
- This report.

No changes were made to the WebRTC/USB/multiseat paths, the controller, or the
Python node.

The VNC rows in `docs/parity-matrix.md`,
`docs/phase4-registration-parity.md`, and `docs/phase4-cutover-audit.md` were
updated to describe password authentication and the still-plaintext transport.
`docs/plan-execution-status.md` was not changed in this pass.

## Configuration and safety policy

| Flag / env | Default | Behavior |
|---|---|---|
| `--vnc-server` / `OZMA_VNC_SERVER` | off | Binds only when enabled **and** the capture registry has a source. |
| `--vnc-bind` / `OZMA_VNC_BIND` | `127.0.0.1` | Supplying the flag is the explicit opt-in for a non-loopback address. |
| `--vnc-server-port` / `OZMA_VNC_SERVER_PORT` | `0` | `0` asks the OS for a free port; the real port is the one registered. |
| `--vnc-password-file` / `OZMA_VNC_PASSWORD_FILE` | unset | Read once at startup; enables RFB security type 2. |
| `--vnc-allow-unauthenticated` / `OZMA_VNC_ALLOW_UNAUTHENTICATED` | off | Explicit opt-in for a non-loopback bind **without** a password. |
| `--vnc-max-clients` / `OZMA_VNC_MAX_CLIENTS` | `4` | Bounded 1..=32 concurrent viewers. |
| `--vnc-idle-timeout-seconds` / `OZMA_VNC_IDLE_TIMEOUT_SECONDS` | `0` | `0` disables it; otherwise 1..=86400 seconds without a client message closes the viewer. |
| `--vnc-clipboard` / `OZMA_VNC_CLIPBOARD` | off | Opt-in bridge between RFB cut text and the desktop-profile X11 clipboard. Requires `--vnc-server`. |
| `--vnc-clipboard-max-bytes` / `OZMA_VNC_CLIPBOARD_MAX_BYTES` | `65536` | Bounded 1..=65536 bytes in either direction. Larger `ClientCutText` payloads are consumed and ignored. |

### Security and bind policy

- With no password file, the server offers **security type 1 (None)** and is
  safe only on loopback. The default bind is `127.0.0.1`.
- With a password file, the server offers **only security type 2 (VNC
  authentication)**. It never offers None alongside it, so a client cannot opt
  out of the configured password.
- A non-loopback bind requires either `--vnc-password-file` (preferred) or the
  explicit `--vnc-allow-unauthenticated`. Without one of them, `NodeConfig`
  validation and `VncServerConfig::validate` both return a typed startup error
  (`UnauthenticatedWiderBindRefused`); the node exits instead of silently
  serving an unauthenticated LAN listener.
- A wildcard bind is reported to the controller as the resolved local address,
  never as `0.0.0.0`.
- **Plaintext transport:** RFB VNC authentication proves knowledge of a shared
  password with a DES challenge/response. It does not encrypt pixels, input, or
  clipboard traffic, and classic VNC DES is cryptographically weak by modern
  standards. Do not treat a password as a substitute for a trusted network,
  WireGuard/mesh transport, or SSH tunnel. There is no TLS and no VeNCrypt.

### Password file policy

- The file is read exactly once at startup and trimmed of **one** trailing
  newline (`\n` or `\r\n`) only. Spaces and all other bytes are preserved.
- Empty files (including a file containing only a newline) are rejected.
- RFB VNC authentication uses one 8-byte DES key. Passwords longer than 8 bytes
  are rejected rather than silently truncated, so operators can see that the
  extra bytes would not contribute.
- On Unix the file must be `0600` or stricter. Any group/other permission bit or
  owner-execute bit is a typed error (`VncPasswordError::Permissions`).
- The password value never appears in logs, `Debug`, errors, counters, mDNS TXT
  records, or `/api/v1/connection`. Only `VncServerConfig` knows whether auth is
  enabled (`VncServerHandle::auth_enabled` / log field `security = "vnc-auth"`).

`--vnc-server` on a node with no capture source is a warned no-listener state:
no socket is bound and registration keeps the typed `not-configured` state. It
does not synthesize a frame, a source, or a fake listener.

## Protocol coverage

The server advertises **RFB 003.008** and accepts clients that negotiate 3.3,
3.7, or 3.8.

### Handshake

- Sends the 12-byte version string.
- Reads the client version and accepts only major 3 with minor 3, 7, or 8.
- RFB 3.3: sends one 32-bit security type. Without a password it is `None` (1);
  with a password it is `VNC authentication` (2). Type 2 then performs the
  challenge/response below and sends `SecurityResult`.
- RFB 3.7/3.8: sends a one-entry list — `[1, 1]` for None or `[1, 2]` for VNC
  authentication — reads the client's choice, and requires it to match.
- Security type 2: the server generates a fresh 16-byte challenge with the OS
  CSPRNG, sends it, reads the 16-byte DES response, and compares it to the
  expected value in constant time.
- On success it sends `SecurityResult(0)`. On failure it sends
  `SecurityResult(1)` (RFB 3.8 adds a short reason string) and closes the
  connection. There is no retry loop and no fallback to None.
- Any choice the server did not offer is refused with `SecurityResult(1)`;
  RFB 3.8 also receives a failure reason string, RFB 3.7 only the result.
- The password bytes are padded to 8, zero-padded, and each byte's bits are
  reversed to form the DES key, matching the RFB VNC-authentication convention.
- Reads `ClientInit.shared`.
- Sends `ServerInit` with the **real capture source geometry** and a name of the
  form `Ozma <node> (<source-id>)`. A source whose geometry is unknown at open
  (QMP `screendump`, D-Bus display) completes version/security/ClientInit, then
  waits for its first real frame before `ServerInit`; no size is fabricated.
- The default `ServerInit` pixel format is 32 bpp, 24 depth, little-endian,
  true-colour, RGB order (r16/g8/b0).

### Client messages

| Type | Message | Behavior |
|---|---|---|
| `0` | `SetPixelFormat` | Accepts 32-bit true-colour with 8 bits per channel and either RGB (r16/g8/b0) or BGR (r0/g8/b16) shifts, little- or big-endian. Any other format closes the connection; RFB has no per-message error, and continuing would send pixels in a format the client did not request. |
| `2` | `SetEncodings` | Parsed with a 1024-entry bound. Every supported entry is kept in the client's stated order: Raw (0), Hextile (5), ZLib (6), Tight (7), and ZRLE (16). The first usable entry is used; if it fails during encoding only that encoding is disabled and the next listed encoding (or Raw) is used. Tight pseudo-encodings are remembered per client: JPEG quality `-32..-23` (0..9, later entries win) and compression level `-256..-247` (0..9). The Cursor pseudo-encoding (-239) and ExtendedDesktopSize (-308) are remembered. Other pseudo-encodings are ignored, and an empty or all-unsupported list falls back to Raw. |
| `3` | `FramebufferUpdateRequest` | Full (`incremental=0`) or incremental (`incremental=1`), with bounded rectangle clipping. Multiple pending requests are merged into at most eight rectangles. |
| `4` | `KeyEvent` | Keysym → USB HID usage, forwarded to the shared `InputSink`. Unknown keysyms are counted and dropped rather than guessed. |
| `5` | `PointerEvent` | Absolute pointer position (scaled to the sink's 0..=32767 report space) plus button/wheel transitions. |
| `6` | `ClientCutText` | Parsed up to 1 MiB. With `--vnc-clipboard` on a desktop/X11 node, the ISO-8859-1 text is sanitised (NUL/C0/C1 controls removed except tab/CR/LF) and written through the desktop clipboard path when it fits `--vnc-clipboard-max-bytes`; otherwise it is consumed and counted as rejected. With the bridge off it is parsed and ignored. A length above 1 MiB closes the connection instead of trusting the field. |
| `251` | `SetDesktopSize` | Parsed with a bounded one-screen policy. The capture source cannot be resized by VNC, so a structurally valid layout is denied (`Prohibited`); zero-sized/screenless/too-many-screen layouts get `InvalidScreenLayout`, and over-16-megapixel layouts get `OutOfResources`. Every screen record is consumed. The client's current geometry is echoed in an ExtendedDesktopSize rectangle with reason=1 (client) only when the client advertised -308; otherwise the denial is counted with no wire reply. The server never applies a size it did not obtain from the capture source. |
| other | unknown | The connection is closed with a typed protocol diagnostic; the RFB framing cannot be recovered safely. |

### Framebuffer updates

- Each update is `FramebufferUpdate` + one or more rectangles. Rectangles use
  Raw (0), Hextile (5), ZLib (6), Tight (7), or ZRLE (16) according to the
  client's `SetEncodings` preference order. A wide Tight rectangle is split
  into several bounded wire rectangles, so one request can yield more than one
  rectangle.
- Raw rectangles carry 32-bit pixels in the client's negotiated channel order.
  RGB24 and BGRA capture sources are encoded directly; no color conversion
  library or fake intermediate format is used.
- Full requests send the latest frame even if unchanged. Incremental requests
  wait for a newer generation. If the client requested Cursor, the one-time
  empty cursor rectangle may accompany an otherwise unchanged incremental
  response without re-sending pixel data.
- `ClientCutText` and unsupported encodings do not desynchronise the message
  stream.

### Desktop resize (ExtendedDesktopSize)

- Geometry is no longer frozen forever after `ServerInit`. The capture pump
  accepts a new frame whose width/height changed, as long as the pixel layout is
  unchanged and the geometry is non-zero, at most `u16::MAX` in each dimension,
  and at most 16 megapixels (64 MiB at four bytes per pixel). Invalid or
  oversized frames are dropped and counted in `source_errors`; the previous
  framebuffer stays live.
- A viewer that listed **ExtendedDesktopSize (-308)** in `SetEncodings` gets a
  -308 pseudo-rectangle **before** any pixels in the first update that observes
  the new size. The rectangle carries reason=0 (server-initiated), result=0
  (success), the new width/height, and one screen record (id 0, x/y 0, the full
  new size, flags 0). The same update then sends the whole new framebuffer
  through the viewer's selected Raw/Hextile/ZLib/Tight/ZRLE encoder, so no part
  of the resized framebuffer is left undefined.
- The per-client geometry used for request clipping and pointer scaling moves
  to the announced size only after the resize rectangle has been written.
  ZRLE, ZLib, and Tight per-client zlib streams are **not** reset by a resize;
  the new rectangle simply continues the existing stream.
- A client that did **not** advertise -308 keeps its `ServerInit` geometry. A
  grown frame is clipped to that old size, and a shrunk frame clips requests to
  the new smaller size; the viewer is never sent out-of-bounds pixels or a
  pseudo-encoding it did not request. Those clients still need a reconnect to
  see the new size, but an existing session is not desynchronised.
- `SetDesktopSize` is never treated as a server resize command. The capture
  source exposes no resize API, so the server cannot truthfully accept it.

### Encoding preference and fallback

- The client's `SetEncodings` order decides between all supported encodings.
  Duplicates are collapsed and unsupported entries are ignored; the first
  advertised encoding that has not failed is used. With no advertised
  supported encoding, Raw is used.
- If an encoder returns an error, that rectangle and the rest of the update
  fall back to Raw, and only that encoding is disabled for the rest of the
  connection. If the client advertised another supported encoding, later
  updates use it; otherwise they stay on Raw.
- ZRLE, ZLib, and Tight keep **separate** per-client zlib streams. A failure
  in one does not reset or disable the others, and switching between them never
  mixes stream state. Within Tight, a JPEG failure disables only the JPEG
  sub-path; lossless Tight keeps using the same zlib stream. A change to the
  negotiated compression level starts a new Tight zlib stream and sets the
  stream-0 reset bit on the next Tight rectangle, so the decoder discards its
  old inflate state instead of misreading the new stream.
- Raw is always legal in RFB, so a client that advertised only a compressed
  encoding still receives a valid Raw rectangle when compression fails.

### Hextile

Hextile (5) is used only when the client lists it before the other usable
encodings. It uses the RFC 6143 layout:

- 16x16 tiles, left-to-right and top-to-bottom, with smaller edge tiles when
  the rectangle is not a multiple of 16.
- The encoder chooses the smaller of a Raw tile and a
  background/foreground-plus-subrect tile. A solid tile that already matches
  the current background is one zero subencoding byte; a new solid colour is a
  BackgroundColour subencoding.
- The background is the most frequent tile colour, keeping the previous
  background on a tie. Remaining pixels become maximal same-colour rectangles
  (up to 16x16 pixels). All subrects share the ForegroundColour subencoding
  when they have one colour, otherwise every subrect uses the SubrectsColoured
  form with its own PIXEL.
- Background and foreground colours persist across tiles in one rectangle,
  and are explicitly specified until the client has learned them. They are
  reset at the start of every rectangle; a Raw tile does not change them.
- Subrect coordinates are tile-relative; width and height are encoded as
  `value - 1` in the high/low nibbles. Subrects never cross their tile.
- PIXEL is four bytes for the accepted 32-bit true-colour formats; RGB and BGR
  channel order, little- and big-endian are all honoured. Hextile does not use
  ZRLE's three-byte CPIXEL.

### ZLib

ZLib (6) is used only when the client lists it before the other usable
encodings.

- One zlib stream per client, separate from the ZRLE stream, continued across
  rectangles and updates. Each rectangle starts with the 4-byte compressed
  length and is sync-flushed after the rectangle.
- The uncompressed rectangle payload is exactly the Raw encoding's pixels in
  the client's negotiated format and channel order.
- A single rectangle's compressed payload is bounded by `MAX_ZLIB_RECT_BYTES`
  (64 MiB frame + 8 MiB headroom). An encode error disables ZLib for the rest
  of the connection and falls back to Raw for that update.

### ZRLE

ZRLE is used only when the client listed encoding 16 before the other usable
encodings. It is never sent to a client that did not advertise it.

- One zlib stream per client, created when the viewer's write loop starts and
  continued across rectangles and updates. Each rectangle starts with the
  4-byte compressed length, and the stream is sync-flushed to a byte boundary
  after every rectangle (RFC 6143 section 7.7.6).
- 64x64 tiles, left-to-right and top-to-bottom, with smaller edge tiles when
  the rectangle is not a multiple of 64.
- The tile encoder evaluates Raw, solid, packed palette (2..=16 colours),
  plain RLE, and palette RLE (2..=127 colours) and emits the smallest. Packed
  indices are MSB-first with row padding; runs may cross tile rows; run lengths
  use the RFC's 255-continuation form.
- CPIXEL is three bytes for the accepted 32-bit true-colour formats (depth 24,
  RGB r16/g8/b0 or BGR r0/g8/b16, either endianness), and four bytes if a
  client selects depth greater than 24. Both RGB and BGR wire orders are
  encoded from the same source pixels.
- The palette is never reused across tiles (ZRLE sub-encodings 127 and 129 are
  not emitted). A single rectangle's compressed payload is bounded by
  `MAX_ZRLE_RECT_BYTES` (64 MiB frame + 8 MiB headroom), and tile buffers are
  bounded by 64x64 pixels.
- If a ZRLE encode fails (invalid geometry, compressed-size bound, or zlib
  error), that update falls back to Raw and ZRLE is disabled for the rest of
  the connection, because the partially written zlib stream cannot be trusted.
  The ZLib stream and the client's next preferred encoding remain usable. Raw
  is always legal in RFB, so the client stream stays valid.

### Tight

Tight (7) is used only when the client listed it before the other usable
encodings. It is never sent to a client that did not advertise it. The
implementation covers the subset TightVNC clients need for 32-bit true-colour
sessions:

- **BasicCompression** on zlib stream 0, continued across rectangles and
  updates and sync-flushed after every rectangle. Each compressed payload uses
  Tight's one-, two-, or three-byte compact length (22 bits); a filtered
  payload smaller than 12 bytes is sent uncompressed, as the format requires.
  The zlib level comes from the client's compression-level pseudo-encoding
  (`-256` → level 0, `-247` → level 9) or defaults to 6. A level change starts
  a fresh zlib stream and sets control-bit 0 (reset stream 0) on the next
  Tight rectangle.
- **FillCompression** for a rectangle whose pixels are one colour: a single
  `TPIXEL` after the `0x80` control byte and no zlib data.
- **PaletteFilter** for rectangles with at most 256 colours. The palette size
  byte stores `count - 1`, followed by the palette entries; two colours use
  one bit per pixel, MSB first and row-aligned, and three or more use one byte
  per pixel. The palette header is uncompressed and only the index data goes
  through the zlib stream. The encoder chooses the filter whose pre-compression
  size is smaller; otherwise it considers the copy and gradient filters.
- **GradientFilter** (filter id 2) for high-colour rectangles. Each RGB
  component is replaced by the difference from
  `V[i-1,j] + V[i,j-1] - V[i-1,j-1]`, clamped to 0..255, with pixels outside
  the rectangle treated as zero. The encoder compresses both the copy and
  gradient representations and selects gradient only when it is at least 5%
  smaller, so noise stays on the cheaper copy path. Gradient output keeps the
  same `TPIXEL` length (three RGB bytes for depth 24, a full four-byte PIXEL
  for depth 32).
- **JpegCompression** (`0x90`) for high-colour rectangles, but **only when the
  client advertised a JPEG quality level** with `-32..-23`. The level maps to a
  conservative 20..92 percent JPEG quality (level 0 → 20, level 9 → 92). The
  encoder compares the JPEG payload plus its compact length against the
  lossless payload plus compact length and uses JPEG only when it is smaller;
  ties stay lossless. JPEG data is always a JFIF RGB stream and is never sent
  for Fill/Palette rectangles or for a client without the quality hint. A
  JPEG encode error or oversized payload disables only JPEG for the rest of the
  connection and the same rectangle falls back to lossless Tight.
- `TPIXEL` is three bytes in R, G, B order when the client chose 32-bit
  true-colour with depth 24 and 8-bit channels — independent of the shift order
  and endianness — and a full four-byte `PIXEL` for depth 32. Both RGB and BGR
  client formats are encoded from the same source pixels; JPEG always extracts
  RGB regardless of wire order.
- **Bounds:** TightVNC decoders cap a rectangle at 2048 pixels wide, so a wider
  request is split into multiple rectangles. Each wire rectangle is also capped
  at 1,000,000 pixels, which keeps the uncompressed payload at 4,000,000 bytes
  and the compressed payload inside the 4,194,303-byte compact-length limit even
  for incompressible data. The same compact-length bound is enforced for JPEG
  before it is selected. Rectangle buffers (palette, copy, gradient, RGB, JPEG,
  and predictor rows), the palette lookup, and the per-client stream are all
  bounded by those limits; nothing grows with frame count.
- If a lossless Tight encode fails (invalid geometry, compact-length bound, or
  zlib error), the whole requested rectangle is emitted as Raw and Tight is
  disabled for the rest of the connection, because the partially written zlib
  stream cannot be trusted. A JPEG-only failure does not disable Tight. The
  ZRLE and ZLib streams remain usable. Raw is always legal in RFB, so the
  client stream stays valid.
- **Not implemented:** `BasicCompression Without Zlib` and the extra zlib
  streams 1–3. Those are not advertised or emitted.

### Cursor pseudo-encoding

- A client that advertises Cursor (-239) receives one `Cursor` rectangle in
  its next update. The node has no cursor-shape source, so the rectangle is the
  typed empty cursor: hotspot 0,0, width 0, height 0, and no pixel/bitmask
  payload. It never fabricates a visible pointer shape.
- The empty cursor is sent once per connection. A client that did not request
  Cursor never receives a Cursor rectangle.
- `DesktopSize` (-223) is not advertised. Only `ExtendedDesktopSize` (-308) is
  implemented; clients that do not request it keep their `ServerInit` geometry
  as described above.

## Capture feed and bounds

- One capture source is opened from the node's existing `CaptureRegistry`:
  the first capability found by `scan()`. Per-provider discovery errors do not
  hide a working source from another provider.
- Geometry comes from the source format when it is known, or from the first
  valid frame for dynamic QMP/D-Bus sources. A later frame with a different
  width/height is accepted when it passes the same bounds and the pixel layout
  is unchanged; it is published as a resize (see Desktop resize). A frame with a
  different pixel layout is still dropped, not reinterpreted.
- Frames are copied into a single last-frame snapshot. Viewers always read the
  newest published frame; intermediate frames are counted in
  `frames_coalesced`, not queued.
- Bounded:
  - 16 megapixels / 64 MiB per frame (4K fits; 8K is refused at startup);
  - 16x16 Hextile tiles, 64x64 ZRLE tiles, and 72 MiB compressed-rectangle
    bounds (`MAX_ZRLE_RECT_BYTES`, `MAX_ZLIB_RECT_BYTES`);
  - 255-byte RFB desktop name;
  - 1..=32 concurrent viewers (default 4, `--vnc-max-clients`);
  - optional idle timeout of 1..=86400 seconds
    (`--vnc-idle-timeout-seconds`, default off);
  - 64 pending update-request commands per viewer;
  - 1 MiB `ClientCutText`;
  - 1024 `SetEncodings` entries;
  - 16 screens in one `SetDesktopSize` message (the wire count is consumed up
    to its `u8` maximum, but layouts above 16 are denied as invalid);
  - 5 s handshake timeout and 10 s per-update write timeout. A viewer that
    cannot drain an update is closed and counted as `slow_clients`.
- A frame whose pixel format or byte length no longer matches its format is
  dropped and counted, never reinterpreted. A width/height change with the same
  pixel layout is handled as a bounded resize instead of being dropped.
- Per-viewer resize state is bounded too: one announced geometry, at most one
  -308 rectangle per geometry change, and no extra buffers proportional to
  frame count.
- The capture source is stopped during the cooperative shutdown window; the
  listener and in-flight viewers are aborted only if that window expires.

## Input forwarding

- Key/pointer events use the same shared `InputSink` as the controller's UDP
  HID listener and the session transport.
- ASCII, named X11 navigation/function/keypad/modifier keysyms, and the common
  XF86 audio keysyms are mapped to USB HID usages. Unsupported keysyms are
  counted as `unsupported_keys` and ignored.
- RFB pointer coordinates are framebuffer pixels; the adapter scales them to
  the sink's identity HID report space (`0..=32767`) so the existing
  uinput/gadget/QMP sinks do not interpret them as tiny coordinates.
- RFB wheel buttons 4/5 become `InputEvent::Wheel` detents.
- Every viewer disconnect calls `InputSink::release_all`, including handshake,
  protocol, and write-timeout failures. With more than one viewer the sink is
  still one shared keyboard/pointer, so a disconnect releases the shared state
  for all viewers; this is documented as a limitation below.

## Clipboard bridging (opt-in)

The bridge is off unless `--vnc-clipboard` is set. It uses the same
`DesktopControls` X11 clipboard path as
`GET/POST /clipboard` (`xclip` or `xsel`), so it adds no second process boundary
and inherits the desktop layer's 64 KiB UTF-8/NUL checks. It is only installed
when the node runs `--profile desktop` with an X11 display; on any other profile
the flag is accepted as a warned no-op and status reports the clipboard as
unavailable.

- **Viewer → node:** `ClientCutText` bytes are decoded as **ISO-8859-1**
  (RFB 6143), not UTF-8. C0/C1 controls are stripped except tab, CR, and LF, so a
  viewer cannot inject NUL or terminal-style escapes into the desktop clipboard.
  The text is written only when it fits `--vnc-clipboard-max-bytes`; a larger
  payload is still consumed so later messages stay in sync, then counted as
  rejected. A length above the 1 MiB protocol cap closes the connection.
- **Node → viewer:** once per connection, after `ServerInit`, the server reads
  the node clipboard and sends one `ServerCutText` when the backend is available,
  the text is representable in ISO-8859-1, and it fits the configured bound.
  Characters outside Latin-1 cause no message rather than a silent `?`
  substitution. An empty clipboard is sent as a zero-length `ServerCutText`.
- **No echo:** cut text from a viewer is written to the node clipboard but is
  never broadcast back to that viewer (or any viewer). The only `ServerCutText`
  is the connect-time snapshot.
- **Unavailable or failed:** a missing display/tool, command failure, oversize
  text, or non-Latin-1 text is ignored safely. The framebuffer session continues,
  the viewer is not disconnected, and no sync time/direction is recorded.
- **Status without contents:** `VncServerStats` and
  `VncServerHandle::clipboard_status()` expose `enabled`, `available`,
  `last_direction` (`none`/`viewer-to-node`/`node-to-viewer`),
  `last_sync_unix_ms`, per-direction success counts, and a rejection count. The
  clipboard text never appears in logs, `Debug`, counters, or the status
  structure. Startup logs only `clipboard = disabled|available|unavailable` and
  the configured bound.
- **Not a continuous sync:** the node does not poll the desktop clipboard or
  push later node-side changes to an already connected viewer. Reconnect to pick
  up a new node clipboard value. Clipboard traffic remains inside the same
  plaintext RFB transport described above, so enable this only on a trusted
  link.

## Registration and `/api/v1/connection`

- The internal listener's **actual bound** host/port is resolved before mDNS and
  direct registration. `--vnc-server-port 0` therefore registers the real
  ephemeral port, not `0`.
- `--vnc-server` may rebroadcast any configured capture source, including
  `--capture-provider vnc` (external QEMU), QMP, D-Bus, or a hardware source.
  `--vnc-host`/`--vnc-port` still select the **input** capture endpoint; the
  internal listener is the **advertised** endpoint when it is running.
- `RegistrationExtras::resolve_with_served_vnc` gives the listening internal
  endpoint precedence over `--vnc-host`/`--vnc-port`. The external QEMU endpoint
  remains the fallback when no internal listener is running.
- The same `RegistrationExtras` is attached to
  `/api/v1/connection.registration`, so the controller sees
  `{"vnc":{"state":"configured","host":...,"port":...}}` while listening and
  the typed `not-configured` state with `vnc_host`/`vnc_port` listed as
  unsupported fields otherwise.
- On shutdown the listener task stops the capture source and closes the
  socket. Registration is a startup-time advertisement; a later capture-source
  failure stops the listener but does not retroactively remove the mDNS/TXT
  record (see remaining gaps).

## Tests and evidence

### Automated

`cargo test -p ozma-node --lib vnc` — **77 passed, 0 failed** for the RFB,
resize, clipboard-bridge, config, and registration coverage. The main working
tree currently contains unrelated in-progress edits and a broken `ozma-mesh`
import (`DiscoveryAuthStatus`), so this pass was validated in a clean `HEAD`
worktree (`f6e215b`) with only `ozma-node/src/vnc_server.rs` copied in;
`cargo check -p ozma-node --all-targets` passed there, with only pre-existing
warnings in other crates. The wider full-suite number from an earlier pass was
**552 passed, 0 failed, 4 ignored**; the ignored tests are pre-existing opt-in
WebRTC/GStreamer live tests.

This resize pass changed only `ozma-node/src/vnc_server.rs` and this report. It
did not change `docs/parity-matrix.md` or `docs/plan-execution-status.md`, and
did not touch `ozma-usb`, `ozma-mesh`, `ozma-multiseat`, `ozma-audio`, or
`ozma-softnode`. The main working tree also has unrelated in-progress edits;
those were not touched.

New resize tests in `ozma-node/src/vnc_server.rs`:

- `server_resize_sends_extended_desktop_size_and_resized_pixels_for_all_encodings`
  — for Raw, ZRLE, Hextile, ZLib, and Tight in turn, a live RFB connection
  serves the `ServerInit` 4x2 frame, then the capture source publishes an 8x4
  frame. The next update begins with a -308 rectangle (reason 0/server, result
  0/success, 8x4, one full-screen record) followed by the 8x4 pixel rectangle
  decoded through the same persistent encoder stream as the initial frame. This
  proves the per-client ZRLE/ZLib/Tight streams are not reset by a resize.
- `first_frame_geometry_change_after_server_init_sends_resize` — `ServerInit`
  uses the source's declared 4x2 geometry before any frame exists; the first
  real frame is 8x4, so the first update carries -308 plus the new Raw pixels.
- `resize_is_not_announced_to_a_client_without_extended_desktop_size` — a
  viewer that advertised only Raw never receives -308. Growing to 8x4 is
  clipped to its 4x2 geometry; shrinking to 2x1 clips to 2x1. The connection
  stays valid and `desktop_resize_sent` stays zero.
- `oversized_resize_frames_are_dropped_without_moving_the_framebuffer` — a
  frame wider than the RFB `u16` geometry range increments `source_errors` and
  does not publish a new generation; the old 4x2 framebuffer still serves.
- `set_desktop_size_bounds_are_typed` — zero width/height, zero screens, and
  more than 16 screens are `InvalidScreenLayout`; 65535x65535 is
  `OutOfResources`; a valid 8x4 and the exact 16-megapixel 4096x4096 layout are
  `Prohibited` because the capture source cannot be resized.
- `set_desktop_size_is_parsed_and_denied_without_fake_success` — a live
  `SetDesktopSize` 8x4 receives a -308 rectangle with reason 1 (client),
  result 1 (prohibited), and the **current** 4x2 geometry; zero and oversized
  requests get result 3 and 2. A later 4x2 framebuffer request still returns
  the unchanged pixels. Counters show 3 requests, 3 denials, 0 server resizes.
- `set_desktop_size_without_extended_desktop_size_gets_no_wire_reply` — a
  client that did not advertise -308 gets no pseudo-rectangle at all; its next
  message is the normal Raw update, and the request/denial are still counted.

New encoding tests in `ozma-node/src/vnc_server.rs`:

- `hextile_all_subencodings_decode_in_rgb_and_bgr` — a test-side Hextile
  decoder (Raw, background/foreground, AnySubrects, coloured subrects)
  decodes a crafted 64x32 tile pattern in RGB and BGR order. The pattern is
  designed to produce a solid-background reuse tile, a new background, a
  foreground-only subrect tile, a coloured-subrect tile, and a raw tile.
- `hextile_handles_edge_tiles_and_rectangle_offsets` — a 20x17 rectangle at a
  non-zero offset in a 40x40 frame decodes correctly, covering 16x16 edge tiles.
- `hextile_frames_decode_over_the_wire_in_rgb_and_bgr` — the same Hextile
  pattern crosses a real RFB connection and is decoded from the wire in RGB
  little-endian, BGR little-endian, and BGR big-endian.
- `zlib_stream_encodes_multiple_rectangles_in_rgb_and_bgr` — one persistent
  ZLib encoder/decoder pair processes three successive rectangles in RGB,
  BGR, and a depth-32 format.
- `zlib_and_zrle_use_independent_persistent_streams` — a live connection sends
  two ZLib rectangles, switches to ZRLE, then returns to ZLib; both decoders
  keep their own stream state and every rectangle decodes to the expected
  framebuffer.
- `tight_copy_palette_and_fill_decode_in_rgb_and_bgr` — a test-side Tight
  decoder (FillCompression, CopyFilter, PaletteFilter with 2-colour 1-bit and
  8-colour byte-per-pixel packing) decodes copy, palette-2, palette-8, and fill
  rectangles in RGB little-endian, BGR little-endian, BGR big-endian, and
  depth-32 RGB. One encoder/decoder pair processes three successive rectangles
  per case, proving the per-client Tight zlib stream continues correctly.
- `tight_frames_decode_over_the_wire_in_rgb_and_bgr` — copy-filter and
  palette-filter Tight rectangles cross a real RFB connection and decode from
  the wire in RGB, BGR, BGR big-endian, and depth-32 formats; each format sends
  two updates on the same connection and decoder.
- `tight_and_zlib_use_independent_persistent_streams` — a live connection
  alternates Tight and ZLib rectangles; each decoder keeps its own stream and
  every rectangle decodes to the expected framebuffer.
- `tight_quality_and_compression_pseudo_encodings_are_recorded` — `-32`/`-23`
  and `-256`/`-247` set and update the per-client JPEG quality and zlib level
  (0..9), later entries win, and unrelated pseudo-encodings do not disturb
  them.
- `tight_gradient_filter_wins_on_a_smooth_ramp_and_decodes` — a 64x64 smooth
  colour ramp is encoded with filter id 2 in RGB, BGR, and depth-32 formats and
  decodes exactly through the test-side gradient predictor.
- `tight_copy_filter_stays_selected_for_noise` — high-entropy noise keeps the
  copy filter, proving the gradient choice is not unconditional.
- `tight_jpeg_subencoding_decodes_at_the_requested_quality` — high-colour
  rectangles with quality levels 0 and 9 produce `0x90` JpegCompression
  payloads whose compact length covers the body exactly, decode at 64x64, and
  grow with quality; the same rectangle with no quality hint stays lossless
  and never emits `0x90`.
- `tight_compression_level_controls_the_zlib_payload_size` — levels 0 and 9
  produce different payload sizes and both decode to the same framebuffer.
- `tight_compression_level_change_resets_the_zlib_stream` — after a level
  change the next rectangle sets control-bit 0 and the test decoder resets
  before inflating; unchanged levels do not set it.
- `tight_jpeg_subpath_failure_keeps_lossless_tight` — a disabled JPEG sub-path
  still emits decodable lossless Tight for the same high-colour rectangle.
- `tight_jpeg_frames_decode_over_the_wire_and_follow_quality_hints` — a real
  RFB connection sends lossless Tight without a quality hint, then `0x90`
  JPEG with quality -23 and -32; both JPEG rectangles decode with the
  `jpeg-decoder` crate at the expected dimensions and the high-quality payload
  is larger.
- `tight_sub_rects_respect_the_width_and_area_bounds` — widths 1, 2048, 2049,
  4096, and 65535 split into pieces that all stay within 2048 pixels wide and
  1,000,000 pixels, and the pieces cover exactly the requested area.
- `tight_wire_rectangle_splitting_stays_within_bounds` — a solid 4100x600
  frame is encoded as multiple Tight rect headers/payloads, each within the
  width and area limits, each a FillCompression payload, with total coverage
  equal to the frame.
- `encoding_preference_honours_all_supported_codecs_in_client_order` — every
  single supported encoding is used as advertised; Raw/ZRLE, Hextile/ZLib,
  Hextile/ZRLE, ZLib/ZRLE, and every Tight pairing follow the client's order;
  an unsupported entry is ignored; all-unsupported and empty lists fall back to
  Raw.
- `encoding_selection_skips_only_disabled_encodings` — disabling Hextile selects
  the next listed ZLib (then Tight, then ZRLE), not Raw, and Raw remains
  available.
- `compressed_encoding_failure_falls_back_to_raw_and_disables_only_that_encoding`
  — a frame the encoders cannot read forces ZRLE, Hextile, ZLib, and Tight
  errors; each update is emitted as Raw, reports that one failed encoding, and
  a selection with no usable stream still produces Raw without a new failure.
- `hextile_and_zlib_bounds_are_rejected_before_writing` — zero-sized and
  out-of-frame Hextile/ZLib/Tight rectangles return `InvalidInput` without
  appending bytes or touching a zlib stream.
- `zrle_decodes_the_known_framebuffer_in_rgb_and_bgr_formats` — a test-side
  ZRLE decoder (Raw, solid, packed palette, plain RLE, palette RLE, CPIXEL
  reassembly) decodes a 256x64 pattern in RGB little-endian, BGR little-endian,
  and BGR big-endian. The same connection and decoder process three successive
  ZRLE rectangles, proving the per-client zlib stream continues correctly.
- `cursor_pseudo_encoding_sends_a_typed_empty_cursor_only_when_requested` —
  one 0x0 Cursor rectangle after the pixel rectangle, sent once, and never sent
  to a client that did not request it.
- `oversized_set_encodings_is_rejected_typed` — more than 1024 encodings closes
  the viewer with a protocol error.

Existing tests retained and passing include:

- `handshake_negotiates_none_security_and_real_server_init`
- `password_auth_accepts_the_correct_des_response`
- `password_auth_rejects_a_wrong_response_and_closes_without_retry`
- `password_file_policy_trims_one_newline_and_rejects_empty_oversize_and_wide_modes`
- `password_is_redacted_from_debug_and_status_surfaces`
- `des_known_answer_and_rfb_key_expansion_are_correct`
- `idle_timeout_closes_a_viewer_that_stops_sending_requests`
- `non_loopback_without_password_requires_the_explicit_override`
- `dynamic_source_geometry_is_reported_in_server_init`
- `set_pixel_format_rgb_and_bgr_both_encode_raw_correctly`
- `full_and_incremental_requests_return_raw_frame_bytes`
- `key_pointer_and_cut_text_dispatch_and_disconnect_releases`
- `a_viewer_that_never_sent_input_does_not_release_held_input`
- `unsupported_pixel_format_closes_the_viewer_typed`
- `client_cap_refuses_extra_viewers_and_releases_on_disconnect`
- `slow_client_coalesces_intermediate_frames`
- `slow_viewer_hits_the_write_timeout_and_is_closed`
- `non_loopback_bind_is_refused_without_explicit_configuration`
- `disabled_or_sourceless_config_never_binds_a_fake_listener`
- `registration_reports_the_bound_endpoint_only_while_listening`
- `keysym_mapping_covers_ascii_and_named_keys`
- `version_parsing_rejects_malformed_or_unsupported_strings_without_overflow`
- `pixel_format_validation_is_typed`
- `geometry_and_frame_bounds_are_enforced`
- `client_cut_text_decodes_latin1_sanitizes_controls_and_writes_clipboard`
- `client_cut_text_over_the_configured_bound_is_ignored_without_desync`
- `unavailable_clipboard_ignores_cut_text_and_reports_no_sync`
- `server_cut_text_is_sent_on_connect_as_latin1`
- `server_cut_text_is_skipped_when_the_text_is_not_latin1`
- `a_viewers_cut_text_is_not_echoed_back_to_the_same_viewer`
- `client_cut_text_above_the_protocol_cap_closes_the_viewer`

Registration/config tests:

- `registration::tests::served_vnc_endpoint_precedes_the_external_qemu_endpoint`
- `config::tests::vnc_server_flags_require_enable_and_validate_the_bind_address`
- `config::tests::vnc_clipboard_flags_require_server_and_validate_the_bound`
- `main::tests::vnc_server_flags_are_opt_in_and_auto_port_by_default`

### Live smoke (real clients, synthetic capture)

Node command:

```sh
./target/debug/ozma-node \
  --profile desktop --capture-provider fake \
  --vnc-server --vnc-server-port 15999 \
  --http-port 17382 --hid-udp-port 17331 --cap hid,video
```

Result with the controller's own `asyncvnc` client:

```text
screenshots (720, 1280, 4) (720, 1280, 4) changed_pixels 921600
input_sent_ok
registration_vnc {"state": "configured", "host": "127.0.0.1", "port": 15999}
```

Result with TigerVNC 1.16.2 under Xvfb:

```text
CConn:       Connected to host 127.0.0.1 port 15999
CConnection: Server supports RFB protocol version 3.8
CConnection: Using RFB protocol version 3.8
CConnection: Choosing security type None(1)
```

The viewer decoded frames until the test's 6 s timeout stopped it. The node
log recorded the listener and the real bound endpoint.

### Live clipboard smoke (Xvfb + xclip, 2026-11-25)

Environment: Xvfb `:99` (1024x768x24), `xclip`, a fake 64x48 capture source, and
the `ozma-node` binary built from a clean `HEAD` worktree with only the VNC
clipboard changes applied. The node used `--profile desktop` with ambient
`DISPLAY=:99`, `--vnc-server --vnc-bind 127.0.0.1 --vnc-server-port 15999
--vnc-clipboard`. A small raw-socket RFB 3.8 client performed the handshake and
cut-text exchange; `xclip -selection clipboard -o` read the node side.

| Check | Result | Evidence |
|---|---|---|
| Node starts with a usable backend | **PASS** | log `clipboard="available" clipboard_max_bytes=65536`; `bind=127.0.0.1:15999` |
| `ServerCutText` on connect is Latin-1 | **PASS** | node clipboard `from-node-é`; viewer received `b'from-node-\xe9'` |
| `ClientCutText` writes the node clipboard | **PASS** | viewer sent Latin-1 `from-viewer-é\tX`; `xclip -o` returned the same UTF-8 text |
| Control characters are stripped | **PASS** | viewer sent `b'safe\x00\x01text'`; `xclip -o` returned `safetext` |
| No echo to the same viewer | **PASS** | no `ServerCutText` arrived within 0.5 s after the viewer's cut text |
| Cleanup | **PASS** | node, Xvfb, and xclip selection owner were terminated; no listener remained |

This is a smoke test, not a full clipboard-matrix run: it did not exercise
multiple simultaneous viewers, a clipboard owner changing after connect, or a
non-Latin-1 node clipboard.

### Live ZRLE/Cursor smoke (2026-11-25)

The same fake-source node command was used, with the server started at
`127.0.0.1:15999`. `asyncvnc` sends a one-entry `SetEncodings` list containing
only ZLib (encoding 6), so it correctly received **Raw** and produced
`(720, 1280, 4)` screenshots twice; it also completed key/pointer input.

TigerVNC 1.16.2 under Xvfb (which is what real desktop viewers negotiate)
sent 27 encodings with **ZRLE first and Cursor requested**. The server log
reported:

```text
VNC client encoding preferences applied client=1 count=27 preferred=Zrle cursor=true
```

The viewer then decoded ZRLE frames for the full 8 s test window with no
protocol or zlib error, including the empty Cursor rectangle. This is live
evidence for ZRLE selection and the Cursor pseudo-encoding over a real RFB 3.8
client, not just the in-process decoder test.

### Live Hextile/ZLib smoke (2026-11-25)

The same fake-source node command was used at `127.0.0.1:15999` with
TigerVNC 1.16.2 under Xvfb and an external Python stdlib RFB probe.

TigerVNC's default run advertised 27 encodings in the order
`[Zrle, Hextile, Raw]` plus pseudo-encodings, so the server correctly selected
**ZRLE**. Running with `-AutoSelect=0 -PreferredEncoding=Hextile` changed the
advertised order to `[Hextile, Zrle, Raw]`; the server logged:

```text
VNC client encoding preferences applied client=2 count=27 order=[Hextile, Zrle, Raw] cursor=true
```

and the viewer decoded Hextile frames for the full 6 s test window with no
protocol or decode error.

The external probe connected twice:

- Advertising only Hextile, it received one 1280x720 rectangle with encoding
  **5**. Consuming every tile succeeded, and the fake source produced
  subencodings `[0, 2]` (solid background reuse and BackgroundSpecified),
  which is expected for its mostly solid test pattern.
- Advertising only ZLib, it received one 1280x720 rectangle with encoding
  **6**. `zlib.decompressobj()` returned exactly `1280 * 720 * 4` bytes, so the
  4-byte length prefix, sync flush, and raw-pixel payload all matched.

This is live wire evidence for Hextile and ZLib selection and decoding, not
just the in-process decoders. It does not claim a live run for H.264, which
remains unimplemented.

Earlier in this pass, the `asyncvnc` client advertised only ZLib before ZLib
support existed and therefore received Raw. That historical result is no longer
representative: the same client would now receive ZLib.

### Live Tight smoke (2026-11-25)

The same fake-source node command was used at `127.0.0.1:15999` with a
1280x720 synthetic ramp.

TigerVNC 1.16.2 under Xvfb was started with
`-AutoSelect=0 -PreferredEncoding=Tight`. It advertised 27 encodings with
**Tight first**:

```text
VNC client encoding preferences applied client=1 count=27 order=[Tight, Zrle, Hextile, Raw] cursor=true
```

The viewer decoded frames for the full 10 s test window with no protocol,
zlib, or decode error before the test timeout stopped it. Because Tight was the
first advertised usable encoding, the server selected it.

An external Python stdlib RFB probe then advertised **only** Tight (7) and
sent a full 1280x720 update request. It received exactly:

```text
ServerInit 1280x720 bpp=32 depth=24 big_endian=0 true_colour=1
FramebufferUpdate rectangles 1
rect 0: x=0 y=0 1280x720 encoding=7 control=0x00
  compact length 19864; decompressed 2764800 bytes
chosen encodings [7]
decoded pixels 921600 expected 921600
TIGHT_PROBE_OK
```

`control=0x00` is BasicCompression on zlib stream 0 with the CopyFilter, and
19864 needs the three-byte compact length form. The decompressed size is
exactly `1280 * 720 * 3` (`TPIXEL` is three RGB bytes for depth 24), and the
probe consumed one rectangle covering all 921,600 pixels. This is live wire
evidence for Tight basic compression, the compact length, the per-client zlib
stream, and the copy filter. Fill and palette filters are covered by the
in-process decoder tests.

### Live Tight JPEG/gradient smoke (2026-11-25)

The same `--capture-provider fake` node was rebuilt from this change and run
at `127.0.0.1:15999` with a 640x480 synthetic block source. `RUST_LOG`
kept the encoding debug line visible.

TigerVNC 1.16.2 under Xvfb with `-AutoSelect=0 -PreferredEncoding=Tight`
advertised 27 encodings and the two new Tight hints:

```text
VNC client encoding preferences applied client=1 count=27 order=[Tight, Zrle, Hextile, Raw] cursor=true jpeg_quality=8 compression_level=2
```

The server chose the gradient filter for that source because lossless Tight
was smaller than JPEG at level 8. The viewer decoded frames for the full 8 s
window with no protocol or decode error.

An external Python RFB probe then exercised the sub-encoding choice directly
on the same live server:

```text
tight-only:       control=0x40 filter=2 zlib_bytes=2491 (uncompressed 921600)
jpeg-quality-9:   control=0x40 filter=2 zlib_bytes=2529 (uncompressed 921600)
jpeg-quality-0:   control=0x90 JpegCompression reset=0 jpeg_bytes=6528
jpeg-quality-0:   JPEG decoded size=(640, 480) mode=RGB first_pixel=(222, 39, 165)
```

`0x40 filter=2` is BasicCompression with the GradientFilter. At quality level
0 (pseudo-encoding `-32`) the JPEG payload won and `0x90` JpegCompression
decoded to the exact 640x480 dimensions with PIL. The high-quality run shows
the selection is conditional: when lossless gradient is smaller, JPEG is not
sent even though the client advertised it. This is live wire evidence for the
gradient filter, JPEG sub-encoding, quality-level negotiation (including the
TigerVNC `jpeg_quality=8` hint), and the compression-level hint.

### Live ExtendedDesktopSize / SetDesktopSize smoke (2026-11-26)

The fake-source node was run at `127.0.0.1:15999` with
`--capture-provider fake` (1280x720). A Python stdlib raw RFB 3.8 probe
advertised `SetEncodings` `[Raw (0), ExtendedDesktopSize (-308)]` and sent a
`SetDesktopSize` request for 640x480 with one screen. It received:

```text
ServerInit 1280x720 name=b'Ozma SFFPC (fake0)'
SetDesktopSize reply framebuffer update rects 1
denial rect reason=1 result=1 geom=1280x720 encoding=-308 screens=1
screen id=0 x=0 y=0 1280x720 flags=0
framebuffer rect 1280x720 encoding=0 bytes=3686400
RESIZE_PROBE_OK
```

Reason 1 is a client-initiated reply, result 1 is `Prohibited`, and the
rectangle echoes the real current 1280x720 framebuffer rather than the
requested 640x480. The subsequent full request still returned a Raw 1280x720
rectangle, so the denial did not change the session geometry. The node log
showed the same request as `result=Prohibited`, the denial as sent, and no
`ExtendedDesktopSize resize` line (no source resize occurred).

TigerVNC 1.16.2 under Xvfb connected to the same listener. Its 27-encoding
list was logged with `order=[Tight, Zrle, Hextile, Raw] cursor=true
extended_desktop_size=true jpeg_quality=8 compression_level=2`, and it decoded
frames for the 5 s test window with no protocol, zlib, or decode error before
the timeout stopped it. This is live evidence that a real desktop viewer
negotiates -308 and remains healthy.

**Limitation:** the fake capture source never changes geometry, so this smoke
does not produce a live server→client resize rectangle. That path is covered by
the in-process live RFB connection tests with a test-controlled
`ChannelSource`, which exercise Raw, ZRLE, Hextile, ZLib, and Tight after both
a first-frame and a mid-session resize. No hardware/QMP/D-Bus source resize was
available in this environment.

### Commands

```sh
cargo test -p ozma-node --lib vnc
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check ozma-node/src/vnc_server.rs
```

All passed: **77 vnc tests** (`cargo test -p ozma-node --lib vnc`),
`cargo check -p ozma-node --all-targets` clean apart from pre-existing warnings
in other crates (unused imports in `ozma-pipewire`), rustfmt exit 0. For this
resize pass the commands ran in a clean `HEAD` worktree (`f6e215b`) with only
`ozma-node/src/vnc_server.rs` copied in, because the main working tree carries
unrelated uncommitted `ozma-mesh` changes that do not compile
(`DiscoveryAuthStatus` is missing). The main tree's file was still formatted
with the same `rustfmt --check`. A full `ozma-node` lib run was attempted but
aborted with `SIGBUS` in this environment; the targeted `vnc` filter is green
and is the evidence used here.

## Remaining VNC gaps

- **Authentication is optional and the transport is plaintext.** Security type
  2 is implemented, but it only proves knowledge of a shared password. There is
  no TLS, no VeNCrypt, no encryption of pixels/input, and classic VNC DES is
  weak by modern standards. Loopback remains the safe default; a non-loopback
  bind without a password requires the explicit `--vnc-allow-unauthenticated`
  override. Use a trusted network, WireGuard/mesh, or an SSH tunnel when the
  link is not trusted.
- **No rate limiting or lockout for VNC auth.** Each TCP connection gets one
  challenge/response attempt, so there is no in-connection retry loop, but an
  attacker can still open new connections and guess offline-ish DES responses.
  TLS/VeNCrypt or a network-level control is still required for hostile
  networks.
- **No password rotation while running.** The file is read once at startup; a
  changed password requires a node restart.
- **Tight is a subset.** Raw (0), Hextile (5), ZLib (6), Tight (7), and ZRLE
  (16) are implemented. Tight supports Fill, Palette, Copy, Gradient, and
  JPEG sub-encodings on one per-client zlib stream, plus the JPEG quality
  (`-32..-23`) and compression-level (`-256..-247`) pseudo-encodings. It does
  **not** implement BasicCompression Without Zlib or extra zlib streams 1–3.
  JPEG is lossy and is used only for high-colour rectangles where its payload
  is smaller and the client advertised a quality level. A client that
  advertises only an unimplemented encoding still gets Raw, as RFB permits, and
  the server honours the client's order across all five supported encodings.
  A lossless encode failure disables only the failing encoding and falls back
  to Raw for that update; a JPEG failure disables only JPEG.
- **Cursor is always the empty shape.** There is no cursor-shape source in the
  capture pipeline, so the node tells a requesting client that there is no
  local cursor. It does not inspect QMP/QEMU cursor state or synthesize a
  pointer image.
- **Server-initiated resize only; client resize is denied.** ExtendedDesktopSize
  (-308) is implemented for resize events that come from the capture source.
  `SetDesktopSize` is parsed and always denied because the source exposes no
  resize API; the server never claims success by echoing a requested size. There
  is still no continuous-updates pseudo-encoding and no fence. A client that did
  not advertise -308 must reconnect to see a new geometry; its existing session
  stays valid but is clipped to the old size.
- **Capture-source resize policy is conservative.** Width/height changes are
  accepted only when the pixel layout is unchanged, both dimensions are
  non-zero and at most `u16::MAX`, and the frame stays within 16 megapixels.
  Invalid or oversized frames are dropped and counted; the server does not scale,
  crop-and-pad, or fabricate a geometry. A source that changes pixel layout
  (for example RGB24 to BGRA) is still unsupported and needs a new session.
- **Only packed RGB24/BGRA capture sources.** YUYV, NV12, P010, and compressed
  sources are refused at startup; this server does not convert pixel layouts.
  Raw, Hextile, ZLib, Tight, and ZRLE still send only 32-bit true-colour client
  formats (depth 24/32). Tight uses its 3-byte TPIXEL form at depth 24 and a
  four-byte PIXEL at depth 32; the gradient filter predicts RGB components in
  both cases.
- **No frame-rate pacing in the server.** The capture pump runs at the source's
  own pace even when no viewer is connected; the snapshot is overwritten rather
  than queued.
- **One source, read-only geometry from VNC.** The framebuffer follows the
  capture source's geometry, but VNC cannot command the source to resize. A
  `SetDesktopSize` request is a typed denial, not a remote resolution control.
- **One shared input sink.** Multiple viewers share keyboard/pointer state, and
  any disconnect releases all held input. This is conservative (no stuck keys)
  but not true per-viewer input isolation.
- **Clipboard bridging is opt-in and limited.** It is desktop-profile/X11 only,
  uses one shared node clipboard (no per-viewer ownership), does not poll for
  node-side changes after a viewer connects, and has no rich selection MIME
  types. RFB cut text is ISO-8859-1, so text outside Latin-1 is not sent rather
  than mangled. Clipboard contents travel in the same plaintext RFB transport.
  Status is available through `VncServerStats`/`VncServerHandle::clipboard_status`
  and startup logs, not as a new `/api/v1/connection` sub-document.
- **No capture sharing.** The VNC server opens its own source. HLS, WebRTC, and
  snapshots still open their own sources; a single-open device may allow only
  one of them. A shared capture hub is later work.
- **Startup-time registration.** A runtime capture failure stops the listener but
  the already-advertised mDNS/TXT record is not retracted until the node
  restarts. `/api/v1/connection` also reports the startup registration object,
  not a live listener probe.
- **Supervisor flag forwarding.** `ozma-softnode`/multiseat flag forwarding
  documents and code were out of scope for this pass. A supervised child can
  use the new flags directly, but the supervisor does not yet forward
  `--vnc-password-file`, `--vnc-allow-unauthenticated`,
  `--vnc-max-clients`, `--vnc-idle-timeout-seconds`, `--vnc-clipboard`, or
  `--vnc-clipboard-max-bytes`.
- **Optional live tooling.** The live smoke used `--capture-provider fake`; no
  hardware V4L2, QMP, D-Bus, or guest VNC source was exercised in this task. In
  particular, the fake source cannot resize, so the live run proves -308
  negotiation and `SetDesktopSize` denial but not a live server→client resize;
  that path is covered by in-process RFB connection tests with a scripted
  source.

## Live re-validation with a QMP source (2026-11-25)

This section records a later live check against the real demo QMP head; full
commands and the combined WebRTC/VNC matrix are in
`docs/phase4-live-cutover-report.md`, section
"WebRTC scaling + VNC server re-validation (2026-11-25)".

Environment: HEAD `a7586a3`, `ozma-node` 0.1.0, controller venv at
`/tmp/opencode/webrtc-vnc-reval-venv` (system Python 3.14 + offline uv-cache
packages). The node used the real `--qmp-socket /tmp/ozma-vm1.qmp` capture and
QMP input sink:

```bash
RUST_LOG='info,ozma_input=debug' ./target/debug/ozma-node \
  --name vnc1 --profile vm --qmp-socket /tmp/ozma-vm1.qmp \
  --hid-udp-port 17331 --http-port 17382 \
  --capture-provider qmp --input-provider qmp \
  --vnc-server --vnc-server-port 0 --cap hid,video
```

Results:

| Check | Result | Evidence |
|---|---|---|
| OS-assigned port is the registered port | **PASS** | bound `127.0.0.1:38185`; registration `{"state":"configured","host":"127.0.0.1","port":38185}`; `ss` matched |
| Real RFB 3.8 client | **PASS** | controller `asyncvnc` connected, two frames `(800,1280,4)` |
| RFB key/pointer input reaches the node | **PASS** | `QMP input events sent on shared client` x11; guest screen OCR `localhost login: ab` / `Password:` |
| Controller discovery of served VNC | **PASS** | `/api/v1/nodes` → `vnc1._ozma._udp.local.` `vnc_host=127.0.0.1`, `vnc_port=38185` |
| Controller screen/read consumer | **PASS (transport)** | `POST /api/v1/screen/read` 200, 27 regions; bitmap OCR text is garbage on the VGA console (screen_reader limitation, tesseract reads the same frame) |
| Controller screen/vectors consumer | **PASS** | 200 `{"width":1280,"height":800,...}` |
| Registration retracted when listener stops | **FAIL** | bound 45211 then source ended and listener stopped, but `/api/v1/connection.registration.vnc` still advertised 45211 with no listener |

The last row is a live reproduction of the **Startup-time registration** gap
listed above: the endpoint is correct at bind time, but a later capture-source
failure stops the listener without retracting the advertisement. No product
source was changed and nothing was committed. The VNC node, controller, QEMU
VMs, sockets, temporary venv, and artifacts were cleaned up after the run.
