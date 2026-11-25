# Phase 4 VNC/RFB Server Report

Status: implemented and tested in `ozma-node`. The node can serve its own
opt-in RFB 3.8 endpoint for the controller's VNC consumers instead of only
registering an external QEMU VNC endpoint. Standard RFB VNC password
authentication (security type 2) is now available. The server offers **ZRLE**
(encoding 16), **Hextile** (5), and **ZLib** (6) when the client advertises
them, keeping **Raw** as the always-valid fallback, and answers the **Cursor**
pseudo-encoding with a typed empty cursor. **The transport is still plain TCP:
VNC authentication is not encryption and there is no TLS. ZRLE and ZLib are
compression, not encryption.** No commit was made.

## Scope

Added:

- `ozma-node/src/vnc_server.rs` — a bound RFB listener, handshake, optional
  RFB security type 2 (DES challenge/response), Raw, Hextile, ZLib, and ZRLE
  framebuffer encoders, Cursor pseudo-encoding, client-message parser, input
  adapter, and per-listener counters.
- `ozma-node/Cargo.toml` — the `flate2` dependency for the per-client ZRLE and
  ZLib zlib streams (already present in `Cargo.lock` through the image stack).
- `ozma-node/src/main.rs` — `--vnc-server`, `--vnc-bind`,
  `--vnc-server-port`, `--vnc-password-file`, `--vnc-allow-unauthenticated`,
  `--vnc-max-clients`, and `--vnc-idle-timeout-seconds`, startup/shutdown
  wiring, and endpoint logging.
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
| `2` | `SetEncodings` | Parsed with a 1024-entry bound. Every supported entry is kept in the client's stated order: Raw (0), Hextile (5), ZLib (6), and ZRLE (16). The first usable entry is used; if it fails during encoding only that encoding is disabled and the next listed encoding (or Raw) is used. Unsupported entries (Tight, JPEG, H.264, ...) are ignored, and an empty or all-unsupported list falls back to Raw. The Cursor pseudo-encoding (-239) is remembered. |
| `3` | `FramebufferUpdateRequest` | Full (`incremental=0`) or incremental (`incremental=1`), with bounded rectangle clipping. Multiple pending requests are merged into at most eight rectangles. |
| `4` | `KeyEvent` | Keysym → USB HID usage, forwarded to the shared `InputSink`. Unknown keysyms are counted and dropped rather than guessed. |
| `5` | `PointerEvent` | Absolute pointer position (scaled to the sink's 0..=32767 report space) plus button/wheel transitions. |
| `6` | `ClientCutText` | Parsed up to 1 MiB and ignored; clipboard is not exposed by any input sink. A larger length closes the connection instead of trusting the field. |
| other | unknown | The connection is closed with a typed protocol diagnostic; the RFB framing cannot be recovered safely. |

### Framebuffer updates

- Each update is `FramebufferUpdate` + one or more rectangles. Rectangles use
  Raw (0), Hextile (5), ZLib (6), or ZRLE (16) according to the client's
  `SetEncodings` preference order.
- Raw rectangles carry 32-bit pixels in the client's negotiated channel order.
  RGB24 and BGRA capture sources are encoded directly; no color conversion
  library or fake intermediate format is used.
- Full requests send the latest frame even if unchanged. Incremental requests
  wait for a newer generation. If the client requested Cursor, the one-time
  empty cursor rectangle may accompany an otherwise unchanged incremental
  response without re-sending pixel data.
- `ClientCutText` and unsupported encodings do not desynchronise the message
  stream.

### Encoding preference and fallback

- The client's `SetEncodings` order decides between all supported encodings.
  Duplicates are collapsed and unsupported entries are ignored; the first
  advertised encoding that has not failed is used. With no advertised
  supported encoding, Raw is used.
- If an encoder returns an error, that rectangle and the rest of the update
  fall back to Raw, and only that encoding is disabled for the rest of the
  connection. If the client advertised another supported encoding, later
  updates use it; otherwise they stay on Raw.
- ZRLE and ZLib keep **separate** per-client zlib streams. A ZRLE failure does
  not reset or disable ZLib, and switching between them never mixes stream
  state.
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

### Cursor pseudo-encoding

- A client that advertises Cursor (-239) receives one `Cursor` rectangle in
  its next update. The node has no cursor-shape source, so the rectangle is the
  typed empty cursor: hotspot 0,0, width 0, height 0, and no pixel/bitmask
  payload. It never fabricates a visible pointer shape.
- The empty cursor is sent once per connection. A client that did not request
  Cursor never receives a Cursor rectangle.
- `DesktopSize` (-223) is not advertised: geometry is fixed at `ServerInit`.

## Capture feed and bounds

- One capture source is opened from the node's existing `CaptureRegistry`:
  the first capability found by `scan()`. Per-provider discovery errors do not
  hide a working source from another provider.
- Geometry is frozen from the source format when it is known, or from the first
  valid frame for dynamic QMP/D-Bus sources. Later frames with a different
  geometry are dropped, not reinterpreted.
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
  - 5 s handshake timeout and 10 s per-update write timeout. A viewer that
    cannot drain an update is closed and counted as `slow_clients`.
- A frame whose format or byte length no longer matches `ServerInit` is dropped
  and counted, never reinterpreted.
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

`cargo test -p ozma-node --lib vnc` — **49 passed, 0 failed** for the RFB,
config, and registration coverage. The wider `ozma-node` lib suite is
**552 passed, 0 failed, 4 ignored**; the ignored tests are pre-existing opt-in
WebRTC/GStreamer live tests.

Because the main working tree contained unrelated in-progress changes that do
not compile (`ozma-mesh` references a missing `discovery` module), validation
ran in a detached worktree at the same HEAD with only
`ozma-node/src/vnc_server.rs` replaced. No product change was made outside that
file and this report.

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
- `encoding_preference_honours_all_supported_codecs_in_client_order` — every
  single supported encoding is used as advertised; Raw/ZRLE, Hextile/ZLib,
  Hextile/ZRLE, and ZLib/ZRLE pairs follow the client's order; an unsupported
  Tight entry is ignored; all-unsupported and empty lists fall back to Raw.
- `encoding_selection_skips_only_disabled_encodings` — disabling Hextile selects
  the next listed ZLib (and then ZRLE), not Raw, and Raw remains available.
- `compressed_encoding_failure_falls_back_to_raw_and_disables_only_that_encoding`
  — a frame the encoders cannot read forces ZRLE, Hextile, and ZLib errors;
  each update is emitted as Raw, reports that one failed encoding, and a
  selection with no usable stream still produces Raw without a new failure.
- `hextile_and_zlib_bounds_are_rejected_before_writing` — zero-sized and
  out-of-frame Hextile/ZLib rectangles return `InvalidInput` without appending
  bytes or touching a zlib stream.
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

Registration/config tests:

- `registration::tests::served_vnc_endpoint_precedes_the_external_qemu_endpoint`
- `config::tests::vnc_server_flags_require_enable_and_validate_the_bind_address`
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
just the in-process decoders. It does not claim a live run for Tight, JPEG, or
H.264, which remain unimplemented.

Earlier in this pass, the `asyncvnc` client advertised only ZLib before ZLib
support existed and therefore received Raw. That historical result is no longer
representative: the same client would now receive ZLib.

### Commands

```sh
cargo test -p ozma-node --lib vnc
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check ozma-node/src/vnc_server.rs
```

All passed: **49 vnc tests**, **552 lib tests (4 ignored)**, check clean (one
pre-existing `unused_mut` warning in `controller_compat.rs`), rustfmt exit 0.
The commands were run in a detached HEAD worktree because unrelated
in-progress `ozma-mesh` edits in the main tree do not compile.

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
- **No Tight, JPEG, or H.264.** Raw (0), Hextile (5), ZLib (6), and ZRLE (16)
  are implemented. A client that advertises only an unimplemented encoding
  still gets Raw, as RFB permits, and the server honours the client's order
  across all four supported encodings. An encode failure disables only the
  failing encoding for that connection and falls back to Raw for that update.
- **Cursor is always the empty shape.** There is no cursor-shape source in the
  capture pipeline, so the node tells a requesting client that there is no
  local cursor. It does not inspect QMP/QEMU cursor state or synthesize a
  pointer image.
- **No desktop resize or continuous updates.** No `SetDesktopSize` (-223), no
  Extended Desktop Size, no continuous-updates pseudo-encoding, and no fence.
  Geometry is fixed at `ServerInit`; a resolution change requires a viewer
  reconnect and node restart.
- **Only packed RGB24/BGRA capture sources.** YUYV, NV12, P010, and compressed
  sources are refused at startup; this server does not convert pixel layouts.
  Raw, Hextile, ZLib, and ZRLE still send only 32-bit true-colour client
  formats (depth 24/32).
- **No frame-rate pacing in the server.** The capture pump runs at the source's
  own pace even when no viewer is connected; the snapshot is overwritten rather
  than queued.
- **One source, no resize.** Geometry is fixed at `ServerInit` (or at the first
  dynamic frame). A capture source that changes resolution after that has those
  frames dropped; `SetDesktopSize` and continuous resize are not implemented,
  so the viewer must reconnect after a resolution change and node restart.
- **One shared input sink.** Multiple viewers share keyboard/pointer state, and
  any disconnect releases all held input. This is conservative (no stuck keys)
  but not true per-viewer input isolation.
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
  `--vnc-max-clients`, or `--vnc-idle-timeout-seconds`.
- **Optional live tooling.** The live smoke used `--capture-provider fake`; no
  hardware V4L2, QMP, D-Bus, or guest VNC source was exercised in this task.

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
