# ozma-agent — deprecated

**Do not add features here.** This binary is the legacy desktop-agent skeleton
and is frozen (bug fixes only). Desktop seats are moving to
`ozma-node --profile desktop`; see
[`docs/desktop-profile.md`](../docs/desktop-profile.md) and
[`docs/rust-reconciliation-plan.md`](../docs/rust-reconciliation-plan.md) §3
Phase 3.

The current `capture.rs` still only logs a tick and `mesh.rs` is a TODO. The
binary keeps its five tasks (API, capture, metrics, mesh, registration) alive so
packaging/tests do not break, but it does not provide a real desktop media path.

## What `ozma-node --profile desktop` already covers

- PipeWire video capture with a configurable `Video/Source` target.
- `uinput`-first input with QMP only when configured, then HID gadget, then a
  recording-only fallback that never claims injection.
- PipeWire audio capture target (`default`, named object/node, or existing sink
  monitor) and `--audio-router pipewire|pulseaudio` for `/audio/*`.
- `/health`, `/status`, `/node/status`, `/snapshot`, `/display/snapshot`,
  `/display/info`, `/api/v1/connection`, `/api/v1/capabilities`,
  `/agent/metrics`, `/ui/hints`.
- Real registration capabilities (`capture`, `encode`, `input`, `display`,
  `capture_device`) from providers that actually started.

## Remaining parity items before deletion

These are the gaps that still block deleting this crate. Each needs either a
real provider or a documented product decision:

1. **XDG Desktop Portal ScreenCast consent/selection.** The node can open a
   PipeWire node by id/name, but does not perform the portal D-Bus handshake,
   monitor picker, restore token, or consent UI. Until this exists, an external
   portal client must supply `--desktop-capture-target`.
2. **X11 capture fallback.** No `XGetImage`/XShm provider exists;
   `--capture-provider x11` is a typed startup error. This still matters for
   non-portal X11 sessions.
3. **Desktop audio sink provisioning.** The legacy agent created a virtual
   sink. The node can capture `monitor:<existing-sink>` and route through an
   existing graph, but does not create a desktop null/remap sink itself
   (`ozma-softnode --provision-audio` covers VM sinks only).
4. **`GET /audio/nodes` and `POST /audio/sweep`.** The Python desktop node
   enumerated `pw-dump` audio nodes and proxied a room-correction sweep. The Rust
   node returns typed 501 stubs for both.
5. **Multiseat.** The Python agent has `multiseat/` seat assignment and device
   ownership. The desktop profile is one process/one seat and has no seat
   manager.
6. **Tray, installer, and service integration.** The Python/Rust legacy agent
   owns tray UI, Windows/Linux installers, service units, and update hooks. The
   node is a daemon; packaging parity is not complete.
7. **Windows and macOS capture/input.** The desktop profile's real capture is
   Linux/PipeWire and input is Linux uinput/QMP/HID. The Python agent references
   DXGI/`xcap`-style capture for Windows/macOS. There is no honest non-Linux
   provider in `ozma-node` today.
8. **Mesh/Connect relay.** `ozma-agent::client`/`mesh`/`relay` provide the
   Connect/mesh hooks. These are not folded into the desktop profile; decide
   whether they belong in `ozma-node`, `ozma-mesh`, or a separate relay.
9. **Agent-shaped response fields.** The Python desktop node's `/status` returned
   `{name, audio_sink, screen}` and `/snapshot` returned a bare JPEG/503. The Rust
   `/status` aliases the richer `/api/v1/connection` document; verify no
   controller consumer requires the old exact keys.
10. **UI hints / accessibility.** `/ui/hints` is an honest empty document; the
    Python agent had an in-guest/browser hint path. This remains unimplemented.
11. **Screen-capture HLS/MJPEG shape.** The Python agent exposed an MJPEG/HLS
    screen-capture backend. The node's `/display/mjpeg` is a typed 501; HLS is a
    separate opt-in pipeline over the same capture registry.

Delete this crate only after items 1–3 and 9 have automated coverage (or an
explicit product decision to drop them) and `docs/parity-matrix.md` no longer
lists an `ozma-agent`-only behavior.
