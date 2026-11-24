# Desktop profile (`ozma-node --profile desktop`)

Status: **real runtime for PipeWire capture, uinput/QMP/HID input, PipeWire
audio target selection, snapshots/status, and the controller-compatible
`/audio/*` surface. The XDG Desktop Portal ScreenCast consent/selection flow and
X11 capture are not implemented.** `ozma-agent` remains deprecated until the
parity items at the end of this page are closed.

## Run

```bash
ozma-node --profile desktop \
  --name my-desktop \
  --http-port 7382 --hid-udp-port 7331 \
  --desktop-capture-target node:42 \
  --audio-capture-target monitor:my-sink
```

Defaults: `--desktop-capture-target default`, 1920x1080@30, PipeWire audio
`default`, input chain `uinput -> hid-gadget -> recording`. No QMP sink is used
unless `--qmp-socket`/`--qmp-input-socket` is configured. `--capture-provider
fake` is a dev/test source and is labelled as such; no profile default selects
it.

## Capture

- Real capture is `PipeWireCaptureProvider` with `--desktop-capture-target`:
  - `default` (or `pipewire:default`)
  - `name:<node.name>` / `object:<node.name>` / `pipewire:name:<name>`
  - `node:<id>` / `pipewire:node:<id>`
- `--desktop-capture-remote <name>` selects a non-default PipeWire instance.
- `--desktop-capture-width/height/fps` set the requested mode.
- V4L2 is not registered on a desktop node: a camera is not a screen. X11 is not
  implemented; `--capture-provider x11` is a typed startup error.

### Portal limitation

The XDG Desktop Portal (`org.freedesktop.portal.ScreenCast`) handshake is a
permission/selection front-end. After consent it returns a normal PipeWire
`Video/Source` node; this process does not implement the portal D-Bus handshake
or consent UI. Run a portal client (the desktop session, `xdg-desktop-portal`,
or a dedicated selector), note the returned node id/name, and pass it as
`--desktop-capture-target`. Without that, `default` opens whatever the graph
offers — on a headless host this may be a camera or no node at all, and the node
advertises exactly that rather than pretending to capture a screen.

If discovery fails or the configured target is absent, the node logs the
provider error, advertises no capture/encode/display capability, and keeps
running. A missing capture source never falls back to a fake/synthetic source.

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

## Routes

| Route | Behavior |
|---|---|
| `GET /health`, `/api/v1/health` | Liveness JSON. |
| `GET /status`, `/node/status` | Connection/health summary (`/api/v1/connection` shape). |
| `GET /snapshot`, `/display/snapshot` | One JPEG frame from the first capture source (503 with a typed reason if none/unsupported). |
| `GET /display/info` | Source id/name/geometry and last snapshot error. |
| `GET /api/v1/connection` | Profile, ports, input sink/injection, power, codec, HLS, WebRTC, audio state. |
| `GET /api/v1/capabilities` | Real capture/encode/input/display capability document. |
| `GET/POST /audio/*` | Typed 501 without a router; real router calls with `--audio-router`. |

`/snapshot`, `/status`, `/health`, `/display/*`, `/agent/metrics`, `/ui/hints`,
and `/audio/nodes|sweep` are mounted read-only outside the bearer guard for the
unmodified controller. Mutating routes stay guarded when `--api-token` is set.

## Capability honesty

- `capture` lists only sources the provider actually enumerated. A PipeWire node
  is named/described as the graph reports it; it is never relabelled "screen".
- `encode` is populated only when a capture source exists.
- `display` stays unsupported: no EVDI/KMS provider has passed its probe.
- `input` mirrors the sink that opened, not the protocol's fake capability set.
- Registration `capture_device` is filled with the first discovered source id
  when `--capture-device` was not set explicitly.
- `machine_class` defaults to `workstation`; an explicit `--machine-class` wins.

## Verification

```bash
cargo test -p ozma-node --lib config::tests::desktop platform::tests::desktop
cargo test -p ozma-node --bin ozma-node desktop_capture_and_audio
cargo test -p ozma-node --test desktop_profile
cargo test -p ozma-node -p ozma-capture -p ozma-input
```

`tests/desktop_profile.rs` starts the real binary with fake capture and a
recording input sink, so it needs no PipeWire, `/dev/uinput`, X11, portal, or
privileged port, and injects no desktop input.

## Remaining parity before deleting `ozma-agent`

See [`ozma-agent/README.md`](../ozma-agent/README.md) for the maintained list.
Short version: portal ScreenCast consent, X11 fallback, desktop sink creation,
`/audio/nodes` enumeration, `POST /audio/sweep`, multiseat, tray/installer,
Windows/macOS capture/input, mesh/Connect relay, and any agent-specific response
fields the controller still reads.
