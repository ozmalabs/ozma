# Phase 4 node live-test fixes

Date: 2026-11-26 (Australia/Sydney)
Host: CachyOS, Linux 7.3.5-2-cachyos, x86_64
Checkout: `/home/matt/ozma/ozma`
Base HEAD: `f6d0260feb3b458327d0b1f3da4611c2c5be5943` (no commit created)

This report covers the four defects recorded by the Phase 4 desktop and V4L2
live runs:

1. X11 clipboard writes waited for pipe EOF and leaked the selection owner.
2. `--x11-display` was not passed to clipboard/wallpaper tools.
3. `--capture-device` was advertised but did not select the capture source.
4. HLS refused every V4L2 pixel format it negotiated.

All four are fixed. The original live reports remain as the historical
before-fix evidence; their new follow-up sections point here.

## Scope and safety

- Product changes are limited to `ozma-node/`, `ozma-capture/` (unchanged
  runtime logic), `ozma-encode/` (RGB conversion helper), and docs.
- No commit was created.
- The desktop live run used a private Xvfb `:97` at 800x600x24. The real seat
  display (`:0`/`vt2`) was never addressed.
- The V4L2 live run used two dedicated `v4l2loopback` devices. The node ran in a
  private mount namespace where `/dev/video0` was a second loopback advertised
  as `fake camera first` and `/dev/video42` was the labelled
  `ozma-capture-test` device. The host's real cameras (`/dev/video0-1`) could
  not be opened by the node.
- No real desktop, camera, microphone, audio device, udev rule, group, or
  service was changed. Temporary modules, device nodes, X servers, producers,
  node processes, and `/tmp/opencode/ozma-node-live-fixes` were removed.

## 1. Clipboard write completion and selection-owner lifecycle

### Cause

`xclip -i` (and `xsel -i`) fork a selection owner that inherits stdout/stderr
and keeps them open. `SystemCommandRunner::run` joined stdout EOF, stderr EOF,
and the direct child wait, so it waited for the owner instead of the tool. The
request hit its 5 s deadline and returned `502 clipboard.write_failed` while the
text had already been installed.

### Fix

`ozma-node/src/desktop_compat.rs`:

- Command completion is now the **direct child's exit**. stdout/stderr are read
  concurrently into shared bounded buffers, then drained for at most
  `OUTPUT_DRAIN_GRACE` (150 ms). A pipe held by a forked owner no longer extends
  the command deadline. Background `swaybg` observation uses the same bounded
  drain.
- Clipboard writes run in an isolated process group
  (`CommandRequest::with_process_group`). `CommandOutput::group_id` carries the
  direct child's group id.
- Each write carries a unique `OZMA_CLIPBOARD_OWNER` marker. The node records a
  `ProcessOwner { group_id, marker }` only for the owner tree it started.
- A successful new write replaces the previous owner: the old group is sent
  `SIGTERM`, and `/proc/*/environ` is scanned for the old marker (which also
  catches tools that daemonize into a new session). Processes without this
  node's marker/group are never touched.
- `DesktopControls::shutdown()` terminates the tracked owner and any persistent
  wallpaper process. `main.rs` calls it during graceful shutdown; `Drop` is a
  second safety net.
- A failed write terminates its own candidate owner.

### Evidence

Unit tests (`cargo test -p ozma-node --lib desktop_compat`):

- `real_runner_returns_when_the_direct_child_exits_despite_inherited_pipes`:
  a shell exits while a background `sleep` still holds stdout/stderr. The
  runner returns success in under 2 s instead of timing out.
- `clipboard_write_tracks_one_owner_and_replaces_it_on_the_next_write`: fake
  owner group 4101 is terminated when writer 4102 is installed; shutdown
  terminates 4102. The test also verifies every write receives `DISPLAY`, the
  marker, and `isolate_process_group=true`.
- `real_write_completes_and_keeps_exactly_one_fake_selection_owner`: a fake
  `xclip` forks a pipe-holding child. Two writes complete without timeout; the
  first owner pid is gone after the second write, exactly one owner is alive,
  and shutdown removes the second.

Live X11 (`Xvfb :97`, ambient `DISPLAY` unset):

```text
clipboard write 1: {"ok":true,"written":true,"bytes":8,"tool":"xclip"} 200
xclip owners after write1: [1572893]
clipboard write 2: {"ok":true,"written":true,"bytes":8,"tool":"xclip"} 200
xclip owners after write2: [1573032]
clipboard read: {"ok":true,"text":"ozma-two",...}
direct xclip read: ozma-two
xclip after node SIGTERM: []
```

The owner pid changed and exactly one remained after each write; no owner
survived node shutdown.

## 2. Explicit X11 display propagation

### Fix

- Clipboard read/write spawn `xclip`/`xsel` with
  `DISPLAY=<resolved --x11-display or ambient DISPLAY>`.
- `feh`/`xwallpaper` get the same `DISPLAY`; `swaybg` gets
  `WAYLAND_DISPLAY`. The resolved display always wins over the ambient
  environment, so a node cannot silently apply to another seat.
- `feh` is invoked with `--no-fehbg`, so a node no longer writes the operator's
  `~/.fehbg`.
- `xclip`/`xsel` and `feh`/`xwallpaper` "cannot open display" / "failed to
  connect to Wayland" failures map to the typed `503 desktop.no_display`
  instead of a generic 502. Spawn/IO errors with the same text are also mapped.
- `GET /wallpaper` now returns the resolved `display` and `last_wallpaper`
  alongside the existing `x11_display` / `last_applied` names, so it agrees with
  `/api/v1/connection.desktop_controls`. `verified` remains false.

### Evidence

Unit tests assert the exact child environment:

- clipboard read and write: `[("DISPLAY", ":0")]`
- wallpaper (`feh`): `[("DISPLAY", ":0")]`, argv
  `["--no-fehbg", "--bg-fill", <path>]`
- wallpaper (`swaybg`): `[("WAYLAND_DISPLAY", "wayland-0")]`

Live X11 with `DISPLAY` unset and `--x11-display :97`:

```text
geometry: 200, display :97, screen 800x600
wallpaper POST: 200, argv ["--no-fehbg","--bg-fill",...]
wallpaper GET: display ":97", x11_display ":97",
               last_wallpaper path = last_applied path
root screenshot pixels: (400,300)=(0,0,255), (10,590)=(0,0,255)
~/.fehbg exists: no
```

Unreachable display `--x11-display :98` with no ambient `DISPLAY`:

```text
POST /clipboard -> 503 desktop.no_display
POST /wallpaper -> 503 desktop.no_display
GET  /wallpaper -> 200, display ":98", last_wallpaper null
```

## 3. `--capture-device` selects the registry source

### Fix

`--capture-device` is now a real source selector for host/desktop profiles (a
VM's capture source remains its QMP/VNC/D-Bus display):

- `display_control::open_selected_source(registry, requested)` matches an
  enumerated source by exact id (a V4L2 device path such as `/dev/video42`) or
  advertised name. A missing configured device returns an error that names the
  request and explicitly refuses to fall back to another source.
- `display_control::capability_matches` is shared by the selector, status, and
  tests.
- `SnapshotControl::with_source_id` is now applied in `main.rs`; `/video` uses a
  clone of that control (`streaming()`), so it inherits the selection.
- HLS's source factory, the active WebRTC helper, per-console WebRTC, per-console
  HLS, and the console snapshot broker all call the selected-source helper.
  `ConfigConsoleFactory::source_id()` carries the configured device through the
  console factory.
- `/api/v1/connection.capture` reports the selected capability. If the requested
  device is absent, it reports `available:false` with the requested id rather
  than advertising the first camera.

### Evidence

Unit tests:

- `selected_capture_device_is_opened_by_path_or_name`: a registry containing
  `/dev/video0` first and `/dev/video42` second opens `/dev/video42` by path and
  by name.
- `missing_capture_device_is_typed_and_never_falls_back_to_the_first_camera`:
  `/dev/video99` fails with an error naming it and no `/dev/video0` fallback.
- `snapshot_uses_the_selected_capture_device_not_the_first_camera`
- `snapshot_missing_selected_device_is_typed_and_does_not_fall_back` (503
  `capture.unavailable`)

Live V4L2 (namespace with `/dev/video0` = `fake camera first` enumerated first,
`/dev/video42` = `ozma-capture-test`):

```text
connection.capture: {available:true, kind:v4l2, name:ozma-capture-test,
                     source:/dev/video42}
snapshot: 200 image/jpeg 22741 bytes, 640x480, source_id /dev/video42
video: 356352 bytes, 15 complete JPEG frames
```

Missing device, with the working loopback still present:

```text
connection.capture: {available:false, provider:v4l2, source:/dev/video99,
                     kind:null, name:null}
hls_error: "requested capture source `/dev/video99` is not available;
            refusing to fall back to another capture device"
GET /display/snapshot -> 503 capture.unavailable, details "/dev/video99"
GET /video             -> 503 capture.unavailable, details "/dev/video99"
no playlist written
```

## 4. HLS from V4L2 MJPEG / YUYV / NV12

### Fix

`ozma-encode/src/convert.rs` adds a bounded `to_rgb24(FrameFormat, bytes)`
conversion for:

- `rgb24` / `bgra` (packing),
- `mjpeg` (JPEG decode, geometry checked against the negotiated format),
- `yuyv` (limited-range BT.601 4:2:2),
- `nv12` (limited-range BT.601 4:2:0).

`ozma-node/src/hls.rs` now:

- accepts `mjpeg`, `yuyv`, and `nv12` in `ensure_supported`;
- sets the ffmpeg rawvideo input layout to `rgb24` for those layouts (plus the
  existing BGRA conversion);
- converts each frame through `ozma_encode::to_rgb24` before writing;
- passes P010 through unchanged as `p010le`, keeping the existing behavior;
- still rejects H.264/HEVC/AV1 with a typed `UnsupportedFormat` rather than
  feeding compressed bytes to a rawvideo pipe.

Short buffers, odd YUV dimensions, and decoded-MJPEG geometry mismatches are
typed conversion errors, not silent corruption.

### Evidence

Unit tests:

- `ozma-encode --lib`: `yuyv_black_white_and_chroma_are_converted`,
  `nv12_black_and_white_are_converted`, `mjpeg_is_decoded_to_rgb24`,
  `short_and_mismatched_payloads_are_errors`, plus RGB/BGRA packing.
- `ozma-node --lib hls::tests`:
  `yuyv_frames_are_converted_to_rgb24`, `nv12_frames_are_converted_to_rgb24`,
  `mjpeg_frames_are_decoded_to_rgb24`; the old "YUYV is rejected" test now uses
  H.264 and still asserts no ffmpeg spawn.

Live V4L2 loopback runs (each format fed by ffmpeg at 640x480, node HLS
`libx264`, 10 fps):

| Producer format | HLS status | Newest segment |
|---|---|---|
| MJPEG (`mjpeg`) | `running` | h264 640x480, 10/1 fps, 10 decoded frames |
| YUYV (`rawvideo yuyv422`) | `running` | h264 640x480, 10/1 fps, 10 decoded frames |
| NV12 (`rawvideo nv12`) | `running` | h264 640x480, 10/1 fps, 10 decoded frames |

`/api/v1/connection` reported `"hls":"running","hls_error":null` for all three,
and `GET /stream/stream.m3u8` listed a real segment. This removes the typed
"only rgb24 and bgra are supported" blocker recorded by the original V4L2
report.

Snapshot and `/video` for YUYV/NV12 still return the existing typed
`503 display.unsupported_format` because only the HLS feeder was extended. That
was outside this fix's requested HLS scope and is recorded as a remaining gap.

## Verification commands

```bash
cargo test -p ozma-node -p ozma-capture --lib
cargo check -p ozma-node -p ozma-capture --all-targets
cargo test -p ozma-encode --lib
rustfmt --edition 2021 <changed .rs files>
```

Results:

- `ozma-node` + `ozma-capture` lib tests: 646 node tests and 194 capture tests
  passed (0 failed; 4 node and 12 capture live/hardware tests ignored).
- `ozma-encode`: 73 passed (0 failed).
- `cargo check`: success. The only warnings are pre-existing unused imports /
  dead code in `ozma-pipewire`, not in the changed crates' changed code.

## Remaining gaps

- `xsel` was not installed on this host, so only `xclip` had live owner
  lifecycle coverage. The fake-runner test still asserts the exact `xsel -b -i`
  argv and the real runner test exercises the generic fork-held-pipe case.
- Killing a previous owner is best-effort `SIGTERM`; there is no operator-facing
  status for an owner that ignores `SIGTERM`. The marker/group kill cannot
  target a process that both clears its environment and leaves its group.
- A configured but unreachable X display is still advertised as "available" by
  the path/display-based status probe; the operation itself returns the typed
  `503 desktop.no_display`. Probing the X server during status would violate the
  "advertise without running tools" policy.
- HLS still refuses H.264/HEVC/AV1 V4L2 pass-through; only MJPEG/YUYV/NV12 were
  requested and implemented. V4L2 snapshot/`/video` for YUYV/NV12 remain typed
  503s until those layouts are converted in the JPEG encoder too.
- The live desktop run used Xvfb rather than a physical Xorg because no input
  injection or GPU output was needed for clipboard/wallpaper/geometry. It used
  the same `xclip`/`feh`/`xrandr` binaries and the real command runner.
- Per-console WebRTC/HLS and the console broker are wired to the same selected
  source helper but were not re-run live (they need a multi-console VM
  provider); their unit/compile coverage is unchanged apart from the selection
  plumbing.
