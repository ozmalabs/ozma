# Desktop profile (`ozma-node --profile desktop`)

Status: **real runtime for PipeWire, X11 (`x11grab`), and XDG Portal
ScreenCast desktop capture selection, uinput/QMP/HID input, PipeWire audio
target selection, snapshots/status, display geometry/clipboard/wallpaper
controls, and the controller-compatible `/audio/*` surface. `--capture-provider
x11` now registers the real `X11CaptureProvider` with an explicit
`--x11-display`; `--capture-provider portal` runs the bounded portal
consent/selection handshake in `ozma-node` before capture starts. A
denied/timed-out portal, a missing portal service, or a missing X display is a
warned no-capture state, never a fake source. The remaining portal limits are
the PipeWire fd handoff (the backend has no fd-connect API; compositors that
allow only the portal fd fail with a typed unsupported error) and restore-token
persistence (a returned token is reported but not stored by this node).**
`ozma-agent` remains deprecated until the parity items at the end of this page
are closed.

## Run

```bash
ozma-node --profile desktop \
  --name my-desktop \
  --http-port 7382 --hid-udp-port 7331 \
  --desktop-capture-target node:42 \
  --audio-capture-target monitor:my-sink
```

For a portal ScreenCast session (consent may appear at startup):

```bash
ozma-node --profile desktop --capture-provider portal \
  --portal-source-type monitor --portal-cursor hidden \
  --portal-timeout-ms 180000
```

For a local X11 seat without a portal screen-cast:

```bash
ozma-node --profile desktop --capture-provider x11 \
  --x11-display :0 --x11-width 1920 --x11-height 1080 --x11-fps 30
```

Defaults: `--desktop-capture-target default`, 1920x1080@30, PipeWire audio
`default`, input chain `uinput -> hid-gadget -> recording`. No QMP sink is used
unless `--qmp-socket`/`--qmp-input-socket` is configured. `--capture-provider
fake` is a dev/test source and is labelled as such; no profile default selects
it.

## Capture

The desktop profile selects exactly one capture transport:

- `pipewire` (default) registers `PipeWireCaptureProvider` with
  `--desktop-capture-target`:
  - `default` (or `pipewire:default`)
  - `name:<node.name>` / `object:<node.name>` / `pipewire:name:<name>`
  - `node:<id>` / `pipewire:node:<id>`
- `x11` registers `X11CaptureProvider` (ffmpeg `x11grab`) with an explicit
  `--x11-display`, `--x11-offset X,Y`, `--x11-width`, `--x11-height`,
  `--x11-fps`, and the shared `--ffmpeg-bin`.
- `portal` runs `PortalScreenCast::connect`/`request_stream` in the node
  (`--portal-source-type monitor|window|virtual`, `--portal-cursor
  hidden|embedded|metadata`, `--portal-restore-token`, `--portal-timeout-ms`)
  and registers `PortalCaptureProvider` for the selected node.
- `fake` is the explicit dev/test RGB24 source.
- `--desktop-capture-remote <name>` selects a non-default PipeWire instance
  for both PipeWire and portal capture.
- `--desktop-capture-width/height/fps` set the requested PipeWire/portal mode.

V4L2 is not registered on a desktop node: a camera is not a screen. A typed
configuration error is raised for an unknown provider, X11 without
`--x11-display`, portal/X11 flags outside `--profile desktop`, or conflicting
providers; there is no fake fallback.

### X11 (`x11grab`)

The desktop profile registers `ozma-capture`'s `X11CaptureProvider`
(`CaptureKind::X11`) with `--capture-provider x11`. It requires an explicit
display (for example `:0` or `:0.0`); it never inherits ambient `DISPLAY` or
scans X servers. Enumeration validates the configuration only. Open lazily
spawns:

```text
ffmpeg -hide_banner -loglevel error -f x11grab -i <display>+x,y \
       -pix_fmt rgb24 -f rawvideo -s WxH -r fps -
```

The source reads exactly `width * height * 3` bytes per frame, distinguishes a
clean EOF from a truncated frame, kills/reaps the child on stop (idempotent),
and paces delivery to `fps`. A missing display, absent ffmpeg, or bogus
geometry is a typed capture error, never a fake frame. Startup logs the selected
display/region/mode and warns when the configured ffmpeg binary is not found;
the node stays running without video. The real test is `#[ignore]`d
(`OZMA_X11_TEST=1`).

### Portal ScreenCast

`ozma-capture` exports a bounded `PortalScreenCast` client (Linux, `portal`
feature enabled by default, zbus) implementing:

```text
CreateSession -> SelectSources(monitor|window|virtual) -> Start
              -> streams[node_id] -> OpenPipeWireRemote(fd)
```

With `--capture-provider portal`, `ozma-node` runs
`PortalScreenCast::connect` and `request_stream` (bounded by
`--portal-timeout-ms`, default 180 s) **before** building the capture registry.
The handshake may show the desktop's consent/selection dialog at startup.
Denial/cancellation, timeout, a missing session bus/portal service, or a
malformed/missing result logs a clear warning, leaves the node running with no
capture, and never substitutes X11, a camera, or a synthetic source.
`/api/v1/connection.capture` then reports `available:false` with no source.

On success the selected monitor/window becomes a normal PipeWire `Video/Source`
node. `PortalCaptureProvider` holds the returned fd and the portal D-Bus
connection/session for the lifetime of the capture source, and opens the node
through the existing PipeWire provider (`pipewire:node:<id>`). This crate's
PipeWire backend has no fd-connect API, so a compositor that only permits the
portal fd fails with the underlying typed PipeWire error; with
`require_pipewire_fd` (default `true`) a portal response without a remote fd is
a typed `unsupported` source. There is no silent node-id-only claim. Startup
logs whether an fd is held and whether a restore token was returned.

### Portal limitations

The XDG Desktop Portal (`org.freedesktop.portal.ScreenCast`) handshake is a
permission/selection front-end. The node now runs it, but two limits remain:

- **PipeWire fd handoff.** The portal's `OpenPipeWireRemote` fd is the
  permission-bearing connection. The current PipeWire backend connects by
  session-daemon name and has no fd-connect API, so the provider holds the fd
  and opens the returned node id through the session graph. On a compositor that
  exposes the node only through the portal fd this fails with a typed
  `unsupported`/PipeWire error rather than pretending to capture. The intended
  follow-up is a `require_pipewire_fd`/node-id handoff that teaches the PipeWire
  backend to consume the fd directly.
- **Restore tokens.** `request_stream` reports a `restore_token`, and
  `--portal-restore-token` accepts one for the next handshake, but `ozma-node`
  does not persist the returned token. Consent therefore reappears on restart
  unless the operator stores and passes the token themselves.

If the handshake fails, or a non-portal configured target is absent, the node
logs the provider error, advertises no capture/encode/display capability, and
keeps running. A missing capture source never falls back to a fake/synthetic
source.

## Multi-display and GPU passthrough (VM profile)

These paths belong to `--profile vm`; they do not change desktop/hardware
capture.

- `--display-output <label>:<WxH>[:primary][:<source>]` (repeatable) and the
  indexed form `<index>:<label>:<WxH>[:primary][:<source>]` describe VM console
  heads. They are parsed into `VmDisplayTopology`, validated (unique
  `Console_N`, non-zero geometry, at most one primary), and serialized to
  registration `display_outputs` and `/api/v1/connection.display_outputs`.
  `source` is `qmp`, `dbus`, `vnc`, or `looking-glass`; without it the selected
  provider is used. `--display-outputs-json` remains the raw escape hatch for
  host outputs and conflicts with `--display-output`.
- With no explicit outputs, a configured QMP/D-Bus/VNC/Looking Glass provider
  contributes one implicit console. QMP/D-Bus/Looking Glass geometry is `0x0`
  until the first frame because it is not known at startup; VNC reports its
  configured scaled size.
- Only the selected console has a non-empty `capture_source_id`. D-Bus honours
  `--capture-console N` (falling back to `--dbus-console`); QMP/VNC/Looking
  Glass capture one default head, so a different `--capture-console` is reported
  as `display_capture.limitation` instead of being silently applied. Multiple
  advertised consoles do not imply multiple live encoders.
- **Runtime console switch (D-Bus):** `POST /display/select` (bearer-guarded)
  accepts `{"index": <Console_N>}` or `{"id": "<label|Console_N|source-id>"}`.
  The node validates the index against `display_outputs`, rebuilds the capture
  registry for that console, and restarts HLS and WebRTC only if they were
  running. A successful switch updates `/api/v1/connection.display_outputs` and
  `.display_capture.active_console`/`provider` and returns the new outputs plus
  `restarted: {hls, webrtc}`. The HLS replacement generation appends to the
  existing playlist with `#EXT-X-DISCONTINUITY` and a non-decreasing media
  sequence, so a console switch does not truncate the playlist. A failed
  pipeline restart restores the previous registry/console and returns a typed
  502; it never reports success. Selecting the already-active console is an
  idempotent 200 with `changed:false`. Live two-head evidence (1280x800
  Console_0 login/shell ↔ 720x400 Console_1 second-VGA boot text, including a
  forced-failure rollback) is in
  [`phase3-live-console-switch-report.md`](phase3-live-console-switch-report.md).
- QMP, VNC, and Looking Glass remain single-head: selecting a different console
  returns a typed 409 (`code:"conflict"`, `details.single_head:true`) with the
  provider and requested index, and instructs the operator to restart with
  `--capture-console N`. A node with no VM display answers a typed 501. The
  controller dashboard shows a picker populated from `display_outputs` and the
  limitation; it does not pretend a single-head switch happened.
- GPU passthrough: `--capture-provider looking-glass --looking-glass-shm
  /dev/shm/looking-glass` (or just the SHM flag) registers
  `LookingGlassProvider` with `--looking-glass-fps` (default 30),
  `--looking-glass-id`, and `--looking-glass-name`. Discovery reads only the
  KVMFR header; a missing file, foreign magic, or region without a published
  frame advertises no source. There is no synthetic fallback and no
  `v4l2loopback`/emulated-display claim. `--capture-provider looking-glass`
  requires `--profile vm` and `--looking-glass-shm`.
- Remaining controller/GPU limits: the default console switch is one active
  capture source at a time. The bounded snapshot broker provides simultaneous
  JPEG previews, and opt-in `--multi-console-hls` provides bounded simultaneous
  HLS encoders for advertised D-Bus consoles. WebRTC is active-console by
  default; opt-in `--multi-console-webrtc` adds bounded simultaneous per-console
  helpers (one socket/shm each) with `console`/`id` offer routing. Only the
  D-Bus provider exposes
  runtime-selectable console indexes; QMP/VNC/Looking Glass still require a node
  restart (or keep the single active HLS path when `--multi-console-hls` is
  requested). WebRTC restarts keep their configured dimensions unless the
  selected output advertises non-zero geometry, so a console with a different
  mode may need matching `--webrtc-width/--webrtc-height`. The live two-head
  QEMU run proved switching to a second emulated VGA head, but that head still
  showed retained early-boot text rather than a guest-programmed live login
  console; a real second virtio-gpu scanout remains unproven. No GPU/VFIO or
  KVMFR host has been exercised in CI.

## Input

- `uinput` is preferred (`/dev/uinput` plus the `input` group or a udev rule).
- QMP is inserted into the chain only when `--qmp-socket` or
  `--qmp-input-socket` is configured.
- `hid-gadget` and `recording` follow. A recording-only fallback accepts events
  for validation/logging but injects nothing; `/api/v1/connection` reports
  `injecting:false`, and the capability document advertises no keyboard/pointer
  modes.
- uinput creates one pointer device in absolute mode, so only absolute pointer
  support is advertised. QMP/HID advertise absolute and relative.

## Audio

- Session audio capture uses PipeWire with
  `--audio-capture-target default|object:<name>|node:<id>|monitor:<sink>`.
  `monitor:<sink>` captures an **existing** sink monitor; it does not create the
  sink.
- `--audio-router none` (default) leaves `/audio/apply|remove|play|stop` at a
  typed 501. `--audio-router pipewire|pulseaudio` drives an existing
  PipeWire/PulseAudio graph through `pactl`/`pw-play`/`paplay`.
- `--audio-sink <name>` is advertised to the controller; `--audio-track-root`
  selects reference tracks.
- `GET /audio/nodes` remains a typed 501: this node does not enumerate the
  PipeWire graph. `POST /audio/sweep` (room correction) is not implemented.

## Desktop controls (geometry, clipboard, wallpaper)

These routes close the `ozma_agent.py` desktop items. They use real X11 tools
through an injectable runner; missing tools or an unreachable display are typed
503s, never success-shaped no-ops.

- `GET /display/geometry` parses `xrandr --display <D> --query` into the rich
  output list plus the Python agent's `screens` shape. `POST /display/geometry`
  validates one output's mode/position/rotation against the advertised modes,
  applies one exact argv, re-queries to verify, and rolls back to the previous
  mode on failure. Reports `applied` only after the read-back matches.
- `GET/POST /clipboard` read/write UTF-8 text through `xclip` or `xsel`, capped
  at 64 KiB. Non-UTF-8/NUL content is a typed 400 and oversize is 413. A write
  succeeds only when the tool exits zero.
- `GET/POST /wallpaper` accepts a local regular image (PNG/JPEG/WebP/BMP/GIF,
  at most 64 MiB) and applies it with `feh`, `xwallpaper`, or `swaybg`. URLs are
  rejected; there is no download path. `GET` reports the last path this node
  applied (`verified:false`) because those tools have no read-back command.
- `GET /api/v1/connection.desktop_controls` advertises geometry/clipboard/
  wallpaper availability by looking up tools in `PATH`; it never runs them. The
  display is `--x11-display`, then ambient `DISPLAY`; wallpaper can also use
  `WAYLAND_DISPLAY` with `swaybg`. Other profiles answer typed
  `desktop.not_enabled` 501.
- The X11 display is explicit or ambient for these controls. Unlike capture,
  the node never scans X servers. A Wayland-only seat without XWayland gets a
  typed 503 for clipboard/geometry.
- All six routes are mounted in `compat_router`, behind the same bearer guard as
  `/input/*`, `/power/*`, `/audio/*`, `/usb/*`, and `/display/select`.

See [`phase4-desktop-compat-report.md`](phase4-desktop-compat-report.md) for the
argv, rollback, validation, and live-Xvfb evidence.

## Routes

| Route | Behavior |
|---|---|
| `GET /health`, `/api/v1/health` | Liveness JSON. |
| `GET /status`, `/node/status` | Connection/health summary (`/api/v1/connection` shape). |
| `GET /snapshot`, `/display/snapshot` | One JPEG frame from the first capture source (503 with a typed reason if none/unsupported). |
| `GET /display/info` | Source id/name/geometry and last snapshot error. |
| `POST /display/select` | Select an advertised VM `Console_N` at runtime (D-Bus). Typed 409 for QMP/VNC/Looking Glass, 501 without a selector, 502 on a failed restart with rollback. Bearer-guarded. |
| `GET/POST /display/geometry` | Query or set one X11 output's mode/position/rotation through `xrandr`; typed 503 without a display/tool, 400 for an unadvertised mode, 502 on apply/verification failure with rollback. Bearer-guarded. |
| `GET/POST /clipboard` | Read/write bounded UTF-8 text through `xclip`/`xsel`; typed 503 if absent, 413/400 for oversize/binary text, 502 on tool failure. Bearer-guarded. |
| `GET/POST /wallpaper` | Report the last applied path or set a validated local image through `feh`/`xwallpaper`/`swaybg`; URLs rejected, typed 503 without a tool/session. Bearer-guarded. |
| `GET /api/v1/connection` | Profile, ports, input sink/injection, power, codec, HLS, WebRTC, audio state, live `display_outputs`/`display_capture`, and the actual capture provider/kind/source. |
| `GET /api/v1/capabilities` | Real capture/encode/input/display capability document. |
| `GET/POST /audio/*` | Typed 501 without a router; real router calls with `--audio-router`. |

`/snapshot`, `/status`, `/health`, `/display/snapshot`, `/display/info`,
`/agent/metrics`, `/ui/hints`, and `/audio/nodes|sweep` are mounted read-only
outside the bearer guard for the unmodified controller. The desktop controls
(`/display/geometry`, `/clipboard`, `/wallpaper`) and every other mutating route
stay guarded when `--api-token` is set.

## Capability honesty

- `capture` lists only sources the provider actually enumerated. A PipeWire node
  is named/described as the graph reports it; it is never relabelled "screen".
  X11 reports `CaptureKind::X11` with the configured display's mode; a portal
  stream reports the selected node as `CaptureKind::PipeWire` under the
  `portal0` source id.
- `/api/v1/connection.capture` reports the configured transport
  (`pipewire`, `x11`, `portal`, `fake`, or `none`), the enumerated kind/source
  id/name, and `available`. A denied portal or missing X display leaves
  `available:false` and nulls the kind/source.
- `encode` is populated only when a capture source exists.
- `display` stays unsupported: no EVDI/KMS provider has passed its probe.
- `input` mirrors the sink that opened, not the protocol's fake capability set.
- Registration `capture_device` is filled with the first discovered source id
  when `--capture-device` was not set explicitly.
- `machine_class` defaults to `workstation`; an explicit `--machine-class` wins.
- `desktop_controls` is a `PATH` lookup plus the resolved display, not a probe
  command. It reports `available:false` with a reason when a tool or session is
  absent; it never claims a geometry/clipboard/wallpaper change happened.

## Verification

```bash
cargo test -p ozma-node --lib desktop
cargo test -p ozma-node --lib desktop_compat
cargo test -p ozma-node --lib portal_tests
cargo test -p ozma-node --bin ozma-node desktop_x11
cargo test -p ozma-node --test desktop_profile
cargo test -p ozma-node -p ozma-capture -p ozma-input
cargo test -p ozma-capture x11::
cargo test -p ozma-capture portal::
```

`OZMA_DESKTOP_LIVE=1 cargo test -p ozma-node --lib
live_xvfb_geometry_query_and_idempotent_set -- --ignored` starts a throwaway
`Xvfb`, queries geometry, and performs an idempotent set on that private
display; it never attaches to the operator's desktop.

`tests/desktop_profile.rs` starts the real binary with fake capture and a
recording input sink, so it needs no PipeWire, `/dev/uinput`, X11, portal, or
privileged port, and injects no desktop input. The X11 and real-portal tests are
`#[ignore]`d (`OZMA_X11_TEST=1`, `OZMA_PORTAL_TEST=1`, and
`OZMA_PORTAL_CONSENT=1` for the consent flow). The node-level portal tests use a
scripted transport for denial/timeout/missing-service fallback and a fake
`PortalStream` for the success registry; the missing-fd case asserts the typed
`unsupported` error.

## Remaining parity before deleting `ozma-agent`

See [`ozma-agent/README.md`](../ozma-agent/README.md) for the maintained list.
Display geometry, clipboard, and local wallpaper are now covered by
[`phase4-desktop-compat-report.md`](phase4-desktop-compat-report.md) (with the
remaining Wayland clipboard/read-back limits documented there).
Short version: PipeWire fd-connect for portal-only compositors, portal
restore-token persistence, desktop sink creation, `/audio/nodes` enumeration,
`POST /audio/sweep`, multiseat node/controller integration (the
`ozma-multiseat` planner and subprocess supervisor/CLI exist; see
[`multiseat-rust.md`](multiseat-rust.md)), tray/installer, Windows/macOS
capture/input, mesh/Connect relay, and any agent-specific response fields the
controller still reads.
