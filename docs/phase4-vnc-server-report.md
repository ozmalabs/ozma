# Phase 4 VNC/RFB Server Report

Status: implemented and tested in `ozma-node`. The node can now serve its own
opt-in RFB 3.8 endpoint for the controller's VNC consumers instead of only
registering an external QEMU VNC endpoint. No commit was made.

## Scope

Added:

- `ozma-node/src/vnc_server.rs` — a bound RFB listener, handshake, Raw
  framebuffer encoder, client-message parser, input adapter, and per-listener
  counters.
- `ozma-node/src/main.rs` — `--vnc-server`, `--vnc-bind`, and
  `--vnc-server-port`, startup/shutdown wiring, and endpoint logging.
- `ozma-node/src/config.rs` — validation and storage for the new flags.
- `ozma-node/src/registration.rs` — served-endpoint precedence over an external
  QEMU endpoint.
- `ozma-node/src/lib.rs` and `ozma-node/src/platform.rs` — module export and
  test-config field updates.
- This report.

No changes were made to `docs/parity-matrix.md` or
`docs/plan-execution-status.md`. `docs/phase4-registration-parity.md` and the
VNC row in `docs/phase4-cutover-audit.md` still describe the pre-server state
("external QEMU endpoint only"); they are intentionally left for the separate
parity-doc pass because this task's scope was one new report.

## Configuration and safety policy

| Flag | Default | Behavior |
|---|---|---|
| `--vnc-server` | off | Binds only when enabled **and** the capture registry has a source. |
| `--vnc-bind` | `127.0.0.1` | Supplying the flag is the explicit opt-in for a non-loopback address. |
| `--vnc-server-port` | `0` | `0` asks the OS for a free port; the real port is the one registered. |

Security is RFB "None" only. There is **no password, TLS, or authentication**.
The listener therefore defaults to loopback and refuses a non-loopback bind
unless the operator passed `--vnc-bind` explicitly. A wildcard bind is reported
to the controller as the resolved local address, never as `0.0.0.0`.

`--vnc-server` on a node with no capture source is a warned no-listener state:
no socket is bound and registration keeps the typed `not-configured` state. It
does not synthesize a frame, a source, or a fake listener.

## Protocol coverage

The server advertises **RFB 003.008** and accepts clients that negotiate 3.3,
3.7, or 3.8.

### Handshake

- Sends the 12-byte version string.
- Reads the client version and accepts only major 3 with minor 3, 7, or 8.
- RFB 3.3: sends the 32-bit security type `None` (1).
- RFB 3.7/3.8: sends `[1, 1]`, reads the client's security choice, and sends
  `SecurityResult(0)` for `None`.
- A non-`None` choice is refused. RFB 3.8 also receives a failure reason
  string; RFB 3.7 receives only the failure result.
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
| `2` | `SetEncodings` | Parsed with a 1024-entry bound. Raw is accepted; every other encoding (Hextile, ZRLE, Tight, ...) is ignored. The server always sends Raw and never claims another codec. |
| `3` | `FramebufferUpdateRequest` | Full (`incremental=0`) or incremental (`incremental=1`), with bounded rectangle clipping. Multiple pending requests are merged into at most eight rectangles. |
| `4` | `KeyEvent` | Keysym → USB HID usage, forwarded to the shared `InputSink`. Unknown keysyms are counted and dropped rather than guessed. |
| `5` | `PointerEvent` | Absolute pointer position (scaled to the sink's 0..=32767 report space) plus button/wheel transitions. |
| `6` | `ClientCutText` | Parsed up to 1 MiB and ignored; clipboard is not exposed by any input sink. A larger length closes the connection instead of trusting the field. |
| other | unknown | The connection is closed with a typed protocol diagnostic; the RFB framing cannot be recovered safely. |

### Framebuffer updates

- Raw encoding only. Each update is `FramebufferUpdate` + one or more Raw
  rectangles (`x`, `y`, `w`, `h`, encoding 0) followed by 32-bit pixels in the
  client's negotiated channel order.
- RGB24 and BGRA capture sources are both encoded directly; no color conversion
  library or fake intermediate format is used.
- Full requests send the latest frame even if unchanged. Incremental requests
  wait for a newer generation.
- `ClientCutText` and unsupported encodings do not desynchronise the message
  stream.

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
  - 255-byte RFB desktop name;
  - 1..=32 concurrent viewers (default 4);
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

`cargo test -p ozma-node --lib` — **461 passed, 3 ignored** (the ignored tests
are pre-existing opt-in WebRTC/GStreamer live tests).

New RFB tests in `ozma-node/src/vnc_server.rs`:

- `handshake_negotiates_none_security_and_real_server_init`
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
log recorded the listener and the real bound endpoint. This proves a real RFB
3.8 client can complete the handshake and decode Raw updates; it is not a
claim that every VNC feature (Tight, cursor pseudo-encoding, resize, auth) is
implemented.

### Commands

```sh
cargo test -p ozma-node --lib
cargo check -p ozma-node --all-targets
rustfmt --edition 2021 --check \
  ozma-node/src/vnc_server.rs ozma-node/src/lib.rs ozma-node/src/config.rs \
  ozma-node/src/platform.rs ozma-node/src/registration.rs ozma-node/src/main.rs
```

All passed; rustfmt exited 0.

## Remaining VNC gaps

- **No authentication or encryption.** Security None only; no password, TLS, or
  VeNCrypt. Loopback is the default invariant, not a security boundary against
  a hostile local user.
- **Raw only.** No Hextile/ZRLE/Tight/JPEG, no cursor pseudo-encoding, no
  `SetDesktopSize`, no Extended Desktop Size, no continuous updates, no fence,
  or other extensions. A client that only supports non-Raw encodings gets Raw
  anyway per RFB, but cannot receive its preferred compression.
- **Only packed RGB24/BGRA capture sources.** YUYV, NV12, P010, and compressed
  sources are refused at startup; this server does not convert pixel layouts.
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
- **Controller docs are stale.** The VNC rows in
  `docs/phase4-registration-parity.md` and `docs/phase4-cutover-audit.md` still
  say the Rust node never serves VNC and only registers an external QEMU
  endpoint. They should be updated in the separate documentation/parity pass.
- **Optional live tooling.** The live smoke used `--capture-provider fake`; no
  hardware V4L2, QMP, D-Bus, or guest VNC source was exercised in this task.
