# Ozma node ↔ Hermes fork: parity, UX, and a Rust-first plan

**Status:** design proposal. No code changes are implied by this document.

**Scope:** what has to be added to `ozma-node` so a node can replace a Hermes
host for the streaming/remote-desktop use case, while keeping Ozma’s hardware
KVM identity, controller model, and privacy posture. Inputs reviewed:

- `~/ozma/ozma/ozma-node` and the Python node it ports (`node/node.py`)
- `~/code/Hermes` (fork `hardwarebob/Hermes`, branch `modular-session-privacy`)
- `~/ozma/ozma-desktop-client`
- The controller’s existing remote-desktop, game-streaming, WebRTC, and
  Moonlight code
- `docs/architecture.md`, `docs/protocols.md`, `docs/security.md`,
  `docs/display-client.md`, and `docs/routing/*`

---

## 1. Short answer

The Rust node is not yet a streaming node. It currently advertises itself over
mDNS, registers with the controller, sends heartbeats, and runs the UAC2 audio
bridge. It has **no live capture path, no encoder, no media transport, no
session API, no pairing/auth, no HID gadget write path, and no diagnostics**
beyond a static `/health`.

Feature parity with the Hermes fork is therefore not “port a few C++ files”.
It is building seven things in Rust, in this order:

1. **A real media pipeline** — capture → encode → packetise → transport, with
   audio and per-track video.
2. **A session model** — sessions, not sockets, are the unit of lifecycle,
   consent, privacy, reconnection, and multi-client control.
3. **One native multiplexed transport** (QUIC/WebTransport + datagrams) with
   adapters for RTP/UDP, RTSP/TCP-interleaved, Unix/SSH, and HLS/MJPEG
   fallback.
4. **Identity and pairing** that a normal person can complete without reading
   a port table: passkey/QR first, short code only as a fallback.
5. **Truthful diagnostics** — prove capture/encode/transport end to end before
   showing a black screen, and explain every failure with an action.
6. **Remote displays** — a paired remote system can become a real extra
   monitor on the host, visible to the OS and normal display tools, not just a
   mirror window.
7. **A compatibility layer** — Hestia and Moonlight wire protocols, plus the
   controller’s existing `/usb`, `/power`, `/stream`, `/input`, `/webrtc`,
   `/display` and `/metrics` expectations.

The highest-leverage UX decision is also the most architectural one: **the
controller should be the control plane, not the media relay on a LAN.** Clients
should connect directly to nodes using a controller-issued session token. That
is what makes latency, CPU cost, privacy, and failover better than the existing
HLS-through-controller path and the Hermes proxy model.

---

## 2. What the Hermes fork actually adds

The fork is not just upstream Hermes. Its branch adds a coherent set of
capabilities that a node must answer, even if the implementation differs.

| Capability | What the fork does | Why it matters to Ozma |
|---|---|---|
| Multi-track video | RTSP `video0..N`, per-track geometry/resolution/FPS/HDR, `x-ss-video[i].*` SDP attributes, shared control/audio/mic | A node with several capture inputs, or a soft node with several displays, should present independent tracks instead of a composed canvas |
| Virtual display drivers | EVDI/Hermes-KMS virtual outputs, multi-output, Hestia `virtual_display` | Closest analogue to a remote monitor; Ozma should make “attach this client as an extra display” the primary UX, not a streaming side effect |
| Native HTTP/3 | QUIC server on one UDP port; reliable `POST /_hermes/tcp/<port>` streams and RFC 9221 datagrams for media; `--h3-proxy` client | One-port remote access through NAT/firewalls without a VPN or port-forward maze |
| Flexible authentication | Local, PAM, SSH public-key challenge (SSHSIG), scoped Bearer tokens; `/api/auth/pam/pair`, `/api/auth/ssh/challenge`, `/api/auth/ssh/verify`, `/api/tokens` | Removes the 4-digit-PIN/certificate-file onboarding as the only path; fits Linux hosts, SSH-only hosts, and automation |
| Flexible transports | `udp`, `tcp_interleaved`, `unix_socket`, `ssh`; Unix socket + stdio proxy; embedded SSH server | Works through `ssh -L`, a bastion, a local socket, or a direct LAN, without changing the media pipeline |
| Modular privacy | `session_privacy_mode` = `auto`, `kwin_per_output_lock`, `mutter_per_output_lock`, `wlroots_per_output_lock`, standalone variants; per-output lock/DPMS; `org.freedesktop.RemoteSessions`; “disconnect on unlock” | Physical monitors lock/sleep while the streamed output stays live; the local user can see that a remote session exists and choose what unlocking does |
| Hestia API | `session/prepare`, `session/stop`, `display/status`, `display/recover`, `client/permissions`, `diagnostics`, `clipboard`, `commands` | A richer client can negotiate a session instead of guessing from RTSP alone |
| Diagnostics and metrics | Preflight checks, real encoder/fallback state, FPS/resolution/dropped frames/latency/bitrate, `/api/metrics`, `/api/hestia/v1/diagnostics`, session list/terminate | Turns “black screen” into “the capture device produced no frames; here is why” |
| Session lifecycle | Reconnection, end reasons, isolated sessions, session list/terminate | Users can come back to a desktop, and an abandoned session is visible and reclaimable |
| Media quality | H.264/HEVC/AV1, HDR10, zero-copy DMABUF, VAAPI intra refresh, FPS limiting, FEC/loss reporting, touch/pen binding | The difference between “works on a lab LAN” and “usable on Wi-Fi and real hardware” |
| Local console UX | Game Mode console, `pending_pair` status, PIN entry without a desktop | Headless/appliance operation, which is exactly the Ozma hardware node’s world |

The fork’s own roadmap is honest about what is still weak: compositor-specific
hacks, virtual-display setup, black-screen failures, isolated-session
rendering, and the config surface. Ozma should not copy those weaknesses.

---

## 3. Current Rust node reality

`ozma-node` today is much smaller than its source tree suggests.

| Area | Current state |
|---|---|
| mDNS `_ozma._udp` | Implemented (`src/mdns.rs`) |
| Controller registration/heartbeat | Implemented (`src/registration.rs`) |
| UAC2 audio gadget bridge | Implemented (`src/usb_audio.rs`) |
| V4L2 enumeration | Implemented (`src/v4l_enum.rs`) but not linked into the binary |
| Capture → ffmpeg → HLS | `src/capture.rs` exists but is not in `main.rs`/`lib.rs`; the task wrapper is a TODO; integration tests skip |
| HLS/HTTP server | `src/serve.rs` exists but is not used; `src/tasks/hls.rs` is a TODO |
| HID UDP receiver | `src/tasks/udp.rs` logs packets and has a TODO for `/dev/hidg0`/`hidg1` |
| HID gadget output | Not implemented in Rust |
| Encoder probing/negotiation | Static `EncoderConfig`; no probe result, no fallback state |
| Audio capture/encode/transport | Not implemented (only the gadget bridge) |
| RTP/RTCP/FEC | Not implemented |
| RTSP server | Not implemented |
| QUIC/HTTP3/WebTransport | Not implemented |
| Session API/state machine | Not implemented |
| Pairing/auth/permissions | Not implemented |
| Multi-track video | Not implemented |
| Privacy/display control | Not implemented |
| Diagnostics/metrics | Static `/health`; no real metrics |
| USB/IP, virtual media | Not implemented |
| Controller/client compatibility | Registration only; the desktop client currently targets a different, partly imagined API |

Two concrete findings:

1. **Most of the “implemented” capture/server code is orphaned.** `capture.rs`,
   `serve.rs`, `hotplug.rs`, and `tasks/*` are not reachable from the binary,
   and their tests explicitly skip. Wiring and testing them is the first
   cleanup, not a later nicety.
2. **The current dependency set is not portable.** `reqwest` is pulled with its
   default native-TLS stack; `cargo check -p ozma-node` fails in this
   environment on `openssl-sys` because the system OpenSSL is newer than the
   crate supports. The node should use `rustls` end to end.

The Python node (`node/node.py`) is the real feature baseline for the hardware
node: HID gadget, V4L2 capture/HLS, UAC2, power, RGB, current sensor,
serial/RPA, USB storage, and controller registration. The Rust port has only
the last item plus audio.

---

## 4. Parity matrix

Priority meanings: **P0** = without it there is no usable replacement;
**P1** = required for a good replacement; **P2** = differentiation / advanced;
**P3** = polish.

| Capability | Fork baseline | Ozma node today | Rust work | Priority |
|---|---|---|---|---|
| Live capture | KMS/EVDI/portal/X11/V4L2, hotplug, format negotiation | Enumeration only | `CaptureSource` providers; MMAP/DMABUF; hotplug; format and frame timing | P0 |
| Encode | NVENC/VAAPI/QSV/V4L2M2M/software, real probe, fallback | Static config | Typed encoder backend + probe with a real frame; runtime fallback; keyframe/intra-refresh control | P0 |
| Audio | Stream audio + mic, Opus/AAC, sink isolation, A/V sync | UAC2 bridge only | ALSA/UAC2 capture, Opus/AAC encode, jitter buffer, sync, mute/mic policy | P0 |
| Video transport | RTP/UDP, FEC, encryption, feedback | None | RTP packetiser for H.264/HEVC/AV1, FEC, RTCP, pacing, keyframe requests | P0 |
| RTSP | Full server, TCP interleaved, multi-track extensions | None | RTSP server state machine, SDP, SETUP/PLAY/TEARDOWN, interleaved framing | P1 (compatibility) |
| HID input | Keyboard/mouse/gamepad/touch/pen; uinput/gadget | UDP TODO | Gadget writer, report encoder, owner/handoff, release-all, per-track coordinates | P0 |
| Session API | Prepare/stop/status/sessions list/terminate | None | `Session` state machine + HTTP/WS API + idempotency + events | P0 |
| Pairing/auth | PAM, SSH key, Bearer, PIN/mTLS | None | Passkey/QR pairing, device identity, scoped tokens, PAM/SSH-key adapters | P0 |
| Diagnostics | Preflight, metrics, fallback reasons, report | `/health` | End-to-end readiness probe; counters; user-facing remediation | P0 |
| Native QUIC transport | HTTP/3 + datagrams + proxy | None | QUIC/WebTransport server/client; datagram channels; connection migration | P1 |
| Multi-track | `video0..N` + per-track control | None | Multiple sources and encoders; SDP/track metadata; per-track input mapping | P1 |
| Remote display / virtual monitor | Virtual-display drivers, multi-output, Hestia virtual_display | None | `VirtualDisplayProvider` (EVDI/KMS/compositor), EDID/mode negotiation, topology, per-display input, stable identity | P1 |
| Remote display, cross-platform | Windows IddCx, macOS CGVirtualDisplay, hardware HDMI sink | None | Signed Windows driver, best-effort macOS provider, programmable-EDID/HPD hardware path | P2 |
| Reconnect/multi-client | Resume, end reasons, session list | None | Resume tokens; viewer/controller roles; expiry; handoff | P1 |
| Privacy/consent | Per-output lock/DPMS, RemoteSessions, disconnect-on-unlock | None | Policy engine; local consent; display/DDC/CEC/softnode backends | P1 |
| Clipboard | Hestia clipboard + permissions | None | MIME-aware clipboard channel, size limits, explicit opt-in | P1 |
| Fallbacks | HLS/MJPEG paths exist in Ozma | HLS skeleton | Managed HLS/LL-HLS/MJPEG fallback for passive/BIOS view | P1 |
| Hestia compatibility | Full v1 surface | None | Adapter over the native session API | P1 |
| Moonlight compatibility | GameStream/Sunshine protocol | Python controller-side implementation only | HTTPS pairing/app list/launch + ENet/RTP adapter | P1 |
| HDR/zero-copy | HDR10, DMABUF, GPU import | None | Capability-gated HDR (P010/Main10) and DMABUF capture where hardware allows | P2 |
| Isolated sessions | Per-client seat/account/compositor | N/A for hardware node; relevant to softnode | Provider trait; implement for KMS/portal softnodes, not as a node prerequisite | P2 |
| App launch/commands | Per-app commands, Gamescope, permissions | Controller scenarios only | Session-scoped command provider for softnodes/agents; never shell by default | P2 |
| USB/IP + virtual media | USB routing in client; controller `/usb` | Not implemented | USB/IP server, gadget mass-storage, media insert/eject, policy | P2 |
| OTA/console | Signed updates, Game Mode console | Registration only | Signed A/B updates, local console/QR, watchdog | P3 |

---

## 5. Target architecture

The node is a **device**, not a desktop application. It should boot into a
small supervisor, survive a crashed media worker, and expose one coherent
control plane. The architecture follows from that:

```
                       ┌──────────────────────────────┐
                       │ Controller (control plane)   │
                       │ identity · policy · consent  │
                       │ session registry · audit     │
                       └──────────────┬───────────────┘
                                      │ signaling, tokens, events
              ┌───────────────────────┴───────────────────────┐
              │                                               │
      ┌───────▼────────┐                             ┌────────▼────────┐
      │ Ozma client    │  direct media (preferred)   │ Ozma node       │
      │ native/browser │◄═══════════════════════════►│ supervisor      │
      │ Moonlight      │  QUIC / RTP / RTSP          │ session broker  │
      └───────┬────────┘                             └────────┬────────┘
              │ fallback: relay · SSH · Unix · controller     │
              └───────────────────────────────────────────────┘
                                      │
                         ┌────────────┴────────────┐
                         │ capture · encode · audio │
                         │ HID · USB · display      │
                         │ virtual-display provider │
                         └──────────────────────────┘
```

### Design rules

1. **Capability over configuration.** The node detects and advertises what it
   can do. Users choose a profile (“low latency”, “quality”), not a codec,
   port, or encoder.
2. **The session is the unit of trust.** A connection is disposable; a session
   has a principal, scope, privacy policy, expiry, and audit trail.
3. **The media path is direct.** The controller issues a token and policy; it
   does not proxy LAN video.
4. **One internal model, many adapters.** Hestia, Moonlight, WebTransport,
   RTSP, HLS, and the controller API are adapters over one session/media core.
5. **Fail visibly and recover automatically.** Every failure has a code, a
   message, and an action. Hotplug and network changes are normal, not errors.
6. **Least privilege.** Gadget writes, DRM, and display control live behind
   small privileged helpers; the network-facing process does not run as root.
7. **Resource budgets are correctness.** Bounded queues, per-session budgets,
   cancellation, no unbounded task growth on a 1 GB SBC.
8. **Compatibility is a mode, not the architecture.** Moonlight ports are
   bound only when replacing a Moonlight host; the native path is simpler.
9. **A remote monitor must be a real monitor where the platform permits it.**
   It belongs in the OS display topology, not in an overlay window. When a
   driver or compositor cannot provide that, the fallback must be explicit.

---

## 6. Crate and module plan

Keep `ozma-node` as the binary/supervisor and split the reusable pieces into
workspace crates so `ozma-agent`/`ozma-softnode` and the desktop client share
the same types.

| Crate | Responsibility |
|---|---|
| `ozma-protocol` | Wire types, capability/version negotiation, error codes, session events, transport descriptors |
| `ozma-auth` | Device identity, pairing, passkeys, PAM/SSH-key verification, scoped tokens, permissions, revocation |
| `ozma-session` | Session state machine, leases, participants, consent, reconnection, multi-client policy, audit, display leases |
| `ozma-media` | Formats, colour/HDR metadata, frame/buffer abstractions, clocks, A/V sync, track kinds (capture vs virtual display), resource budgets |
| `ozma-capture` | `v4l2`, KMS/DRM, PipeWire/portal, X11, DXGI (feature-gated), hotplug, format negotiation |
| `ozma-encode` | VAAPI, NVENC, QSV, V4L2M2M, software; probe/fallback; rate control; keyframes/intra-refresh |
| `ozma-rtp` | RTP packetiser/depacketiser, H.264/HEVC/AV1 payload formats, FEC, RTCP, jitter/loss feedback |
| `ozma-transport` | Multiplexed channels; UDP, QUIC/WebTransport, TCP-interleaved, Unix/SSH, relay; path selection |
| `ozma-rtsp` | RTSP server/client, SDP, interleaved framing, GameStream/Moonlight adapter |
| `ozma-input` | HID gadget, uinput/evdev, report encoding, owner/handoff, per-display coordinate mapping, clipboard |
| `ozma-display` | EDID/HPD, mode/colour control, DDC/CI, CEC, privacy providers, virtual-display providers (EVDI/KMS/compositor/IddCx/HDMI-sink) |
| `ozma-usb` | Gadget configfs, USB/IP, virtual media, device policy |
| `ozma-api` | `axum` REST/WS/SSE, native API, Hestia adapter, OpenAPI schema |
| `ozma-telemetry` | Metrics, traces, readiness probes, report bundle |
| `ozma-client` | Native client library: QUIC transport, RTSP client, pairing, USB/IP, panel-capability advertisement and remote-display rendering; no external `hermes` binary |
| `ozma-node` | Config, supervisor, platform detection, mDNS, controller registration, CLI |

Reuse what exists: `ozma-crypto` for AEAD/replay, `ozma-proto` for HID/VBAN,
`ozma-mesh` for WireGuard, and the controller’s routing vocabulary. Do not
create a second crypto or discovery stack.

### Suggested dependencies

`tokio`, `bytes`, `serde`, `thiserror`, `tracing`, `axum`, `rustls`, `rcgen`,
`quinn`, `wtransport` or `h3`/`h3-quinn`, `v4l`, `alsa`, `nix`, `udev`,
`zbus`, `pam`/`pam-sys`, `ssh-key` + `signature`, `chacha20poly1305`,
`reed-solomon-erasure`, `opus`, `h264-reader`, `drm`/`gbm` (feature-gated),
`openh264`/`rav1e` for pure-Rust test fallbacks, and `ffmpeg-next` or
`gstreamer-rs` as the practical codec backend. Prefer `rustls` everywhere.

---

## 7. Session model

A session is a **lease over node capabilities**, not a TCP connection.

```
Created ──► Offered ──► PendingConsent ──► Preparing ──► Ready
                │              │                │           │
                │              ├─► Rejected     ├─► Failed  ▼
                │              └─► TimedOut     │        Streaming
                └───────────────────────────────┘           │
                                                    Degraded ◄┤
                                                         │    │
                                                       Draining
                                                         │
                                                       Ended
```

Key properties:

- **Principal:** user, device, controller service, or local console.
- **Scope:** which sources, input, audio, clipboard, files, power, and
  commands are allowed.
- **Tracks:** `capture` (an existing screen/HDMI input) or `virtual_display`
  (a connector created by an `ozma-display` provider). A virtual-display
  track owns a stable display id/EDID and an optional topology lease, so the
  OS remembers where it was arranged.
- **Participants:** one `controller` (input owner), zero or more `viewers`,
  optional `recorder`. A second client cannot silently steal input.
- **Resume token:** short-lived, bound to the principal and session; allows
  reconnection without re-pairing.
- **Reason codes:** `client_disconnected`, `idle_timeout`, `consent_denied`,
  `capture_lost`, `encoder_failed`, `transport_lost`, `superseded`,
  `display_removed`, `local_unlock`, `admin_terminated`.
- **Privacy policy:** local display action, on-screen/indicator state,
  consent requirement, “disconnect on local unlock”.
- **Audit:** who started/stopped, from where, with what scope, and why.

This is deliberately better than the fork’s model, where a stream and a
session are often the same thing and multi-client semantics are implicit.
Explicit roles make collaboration, helpdesk, and “watch without controlling”
possible without a second protocol.

---

## 8. Media pipeline

One pipeline per track, with explicit stages:

```
CaptureSource ──► Frame ──► Encoder ──► EncodedFrame ──► Packetiser ──► Transport
      ▲              │          │             │               │
      │              └──────────┴─────────────┴───────────────┘
      │                     feedback: bitrate, FPS, resolution,
      │                     FEC, keyframe, intra-refresh
      └──────────────────── hotplug / format change / recovery
```

### Capture

- V4L2 with MMAP and DMABUF where available; MJPEG, NV12, YUYV, H.264
  passthrough.
- KMS/DRM capture and PipeWire/portal for softnodes.
- Hotplug via udev/inotify; stable device identity; format negotiation;
  timestamps from `CLOCK_MONOTONIC`.
- Frame validity checks: non-zero size, non-black, expected resolution,
  monotonic PTS. A synthetic “encoder works” probe is not enough; the fork
  learned this the hard way.

### Virtual-display sources

A remote monitor is not a special media pipeline. `ozma-display` creates the
OS connector and its scanout is exposed as another `CaptureSource`, with a
stable `DisplayId` and EDID instead of a V4L2 path. That means:

- the same encoder, packetiser, transport, readiness probe, and telemetry
  apply;
- a mode change from the OS is a format change, not a session restart;
- damage/idle detection can avoid encoding an unchanged desktop;
- if the provider is removed while streaming, the track ends with
  `display_removed` and the client is told why.

On hardware KVM nodes the equivalent source may be an HDMI capture device
that the target sees as a physical monitor; the same abstraction applies, but
the provider is the capture card’s EDID/HPD path rather than a software
driver.

### Encode

- Capability probe with a **real captured frame**, then encode a keyframe and
  verify output. Record the chosen encoder and every failed attempt.
- Runtime fallback: hardware → hardware alternative → software, with a
  visible reason.
- Rate control, low-latency presets, keyframe interval, intra-refresh where
  supported, HDR (P010/Main10) where capture and encoder allow.
- Audio: Opus preferred for interactive; AAC for HLS/compatibility; A/V sync
  from capture timestamps, not wall-clock guesses.

### Packetise and protect

- RTP payload formats for H.264/HEVC/AV1; sequence numbers, marker bits,
  SPS/PPS/VPS handling.
- Reed-Solomon FEC compatible with the Moonlight path; adaptive FEC from loss
  feedback.
- RTCP receiver reports, NACK/PLI/FIR, keyframe requests; intra-refresh lets
  loss heal without a keyframe burst.
- Pacing and congestion control; never let video starve control/input.

### Don’t hide the backend

FFmpeg/GStreamer are acceptable **codec backends**; they are not the
architecture. The current pattern of building a long `ffmpeg` argument string
and hoping is exactly what should be replaced by a typed backend with
capability probing and structured errors.

---

## 9. Transport model

Internally, a session has typed channels:

| Channel | Priority | Reliability | Examples |
|---|---|---|---|
| `control` | realtime | reliable | session events, input, clipboard, cursor |
| `input` | realtime | reliable/unreliable | keyboard, mouse, gamepad |
| `video/<track>` | normal | unreliable + FEC | RTP/QUIC datagrams |
| `audio` | high | unreliable + FEC | Opus/AAC |
| `mic` | high | unreliable + FEC | return audio |
| `telemetry` | low | reliable | metrics, reports |
| `bulk` | bulk | reliable | files, USB/IP, firmware |

Adapters:

1. **Native QUIC/WebTransport** (preferred): one UDP port, TLS 1.3, reliable
   streams for control/API, RFC 9221 datagrams for media. Works through NAT
   and browsers; connection migration survives Wi-Fi/cellular changes.
2. **Raw RTP/UDP**: lowest latency on a trusted LAN and the Moonlight path.
3. **RTSP over TCP with interleaved framing**: compatibility adapter for
   RTSP clients, usable through `ssh -L` or restrictive firewalls.
4. **Unix socket + stdio proxy / SSH subsystem**: bastion and local-only
   deployments.
5. **HLS/LL-HLS/MJPEG**: passive viewing, BIOS, low-power nodes, and
   “something is better than black” fallback.
6. **Relay/WireGuard**: WAN when direct connectivity fails; the relay sees
   ciphertext only.

A `TransportDirector` measures RTT, jitter, loss, and throughput, selects the
best path, and can migrate mid-session. Users never choose a port.

**Native startup:** On an authenticated QUIC connection, the client sends a
single session request with its supported protocol version, requested source
and scopes, and supported media profiles. After authorization and any required
consent, the node returns the session ID, granted scopes, selected profile,
track IDs, codec configuration, and channel mapping. The node starts media
with a decodable keyframe on that connection. Pending consent or capture
readiness is reported on the same control stream. Native clients reuse this
contract for later transports. RTP packetization can be reused independently
of RTSP and SDP.

**Port strategy:** keep Ozma’s existing ports (`7331` HID, `6980` VBAN,
`7340` Opus RTP, `7382` HTTP API) and advertise new ones in mDNS TXT
(`quic_port`, `rtsp_port`, `usbip_port`). Do **not** bind Hermes’s
`47984/47989/47990/48010/48443` by default; provide an explicit
`compatibility = "hermes"` profile that maps them for drop-in replacement.

---

## 10. Remote system as an additional monitor

### Goal

A paired remote system — a laptop, tablet, second PC, TV stick, or another Ozma
Display endpoint — can be attached to a host as an **extra monitor**. The host
OS sees a normal display: it appears in Display Settings, `xrandr`, `kscreen`,
`wlr-randr`, GNOME Displays, or Windows Display Settings; it can be arranged,
rotated, scaled, made primary, used for full-screen apps, captured by ordinary
screen-sharing tools, and managed by the normal window manager. The remote
system renders its contents, advertises its panel capabilities, and sends
input back.

This is not “stream an existing monitor into a window”. That is the fallback.
The first-class feature is a **new display connector** whose scanout happens to
be transported over Ozma.

In this section, **host** means the machine whose display server gains the
monitor, and **client** means the remote system showing it. On a hardware KVM
node, the host is the target PC (where the display server runs) and the node is
the capture/transport path; on a softnode/agent, they may be the same machine.

### What “where possible” means

| Host situation | Mechanism | What the OS sees | Status / limit |
|---|---|---|---|
| Linux agent or softnode | EVDI or a KMS virtual-display driver creates a connector; the compositor adopts it | A normal DRM monitor in display settings | Primary path; needs a kernel driver and compositor adoption |
| Linux compositor-native | KWin virtual output, Mutter virtual monitor/headless output, wlroots/Hyprland headless output | A compositor output, usually visible in that compositor’s tools | Useful without a kernel module; not universal across tools/desktops |
| Windows agent | IddCx indirect display driver controlled by the agent | A normal monitor in Windows Display Settings | Planned; requires a signed driver |
| macOS agent | `CGVirtualDisplay` / private CoreDisplay APIs | Usually visible in Displays preferences | Best effort; private API, OS-version sensitive |
| Hardware KVM target, no agent | HDMI sink with EDID/HPD presented by the capture card or an EDID emulator | A physical monitor to the target | Only if the capture hardware supports the mode/EDID/HPD; many dongles are fixed |
| No provider available | Stream an existing display into the client | Not an extra monitor | Explicit fallback; never label it as an additional monitor |

The rule is simple: **if the host OS cannot see it as a display, do not call it
an additional monitor.** The UI must show “Remote display (virtual monitor)”
for the real thing and “Mirror of display 1” for the fallback.

### Architecture

```
remote client panel capabilities
  native mode · modes · HDR · rotation · scale · physical size · touch/pen · audio
        │
        ▼
controller policy ──► session ──► VirtualDisplayProvider ──► OS display server
                                      │  create · EDID · mode        │
                                      │  topology · DPMS · privacy   │ scanout
                                      ▼                              ▼
                                 input mapping ◄── CaptureSource ──► encode
                                                                      │
                                                                      ▼
                                                                  transport
                                                                      │
                                                                      ▼
                                                                  remote client
```

1. The client advertises its panel. The controller/session chooses a display
   profile and asks the node/agent to create a virtual display.
2. The provider publishes an EDID and one or more modes. The host display
   server owns the final mode/layout decision.
3. The display’s scanout is captured like any other source, encoded, and sent
   as a video track. Only damaged/idle-aware frames need encoding.
4. Client input is mapped from display-local coordinates into the host
   topology and injected as absolute/relative input for that display.
5. Provider events (mode change, topology change, DPMS, removal) update the
   session and the client without restarting it unless necessary.

### Providers and contract

`ozma-display` owns the platform providers:

```rust
pub trait VirtualDisplayProvider: Send + Sync {
    fn capabilities(&self) -> VirtualDisplayCaps;
    fn create(&self, spec: VirtualDisplaySpec) -> Result<VirtualDisplayHandle>;
    fn destroy(&self, id: DisplayId) -> Result<()>;
    fn set_mode(&self, id: DisplayId, mode: &DisplayMode) -> Result<()>;
    fn set_dpms(&self, id: DisplayId, state: DpmsState) -> Result<()>;
    fn set_privacy(&self, id: DisplayId, policy: &DisplayPrivacy) -> Result<()>;
    fn topology(&self) -> Result<DisplayTopology>;
    fn events(&self) -> DisplayEventStream;
}
```

Key types:

```rust
VirtualDisplaySpec {
    id: DisplayId,                 // stable Ozma identity
    name: String,                  // "OZMA-<node>-<client>"
    edid_serial: String,           // stable across reconnects
    physical_mm: (u32, u32),
    modes: Vec<DisplayMode>,       // preferred first
    hdr: HdrCapability,
    rotation: Rotation,
    scale: f32,
    audio: bool,
}

DisplayMode { width, height, refresh_hz, hdr, reduced_blanking }
DisplayTopology { displays: Vec<DisplayPlacement> } // x, y, scale, rotation, primary
DisplayEvent { Added, Removed, ModeChanged, TopologyChanged, DpmsChanged, Error }
```

Backends, in order of preference where available:

- **Linux KMS/EVDI**: a real DRM connector. This is the best “normal process”
  result: the desktop’s own display settings manage it. Prefer EVDI for
  availability; add a Hermes-KMS-style zero-copy driver later if it becomes a
  shared Ozma component.
- **Linux compositor-native**: KWin/Mutter/wlroots headless or virtual outputs.
  No kernel module, but compositor-specific and less universal. Detect and
  label capabilities honestly; do not assume `wlr-output-management` can
  create outputs (it cannot in general).
- **Windows IddCx**: signed indirect-display driver, controlled by the agent
  through a narrow IOCTL/named-pipe API. It must expose EDID, modes, HDR,
  rotation, and stable monitor identity.
- **macOS**: best-effort `CGVirtualDisplay`; fall back to mirroring when the OS
  or entitlement situation does not allow it.
- **HDMI sink**: configure EDID/HPD on the capture hardware when supported;
  otherwise present the card’s fixed mode and mark the display as
  “fixed-mode remote monitor”. This is the only path for an agentless target.

### Normal-process management

The value of a real virtual display is that the host OS does the work:

- windows can be dragged to it, maximised on it, and restored by the window
  manager;
- full-screen games and apps can select it as their output;
- ordinary screen capture, screen sharing, colour management, rotation, scale,
  and “make primary” work;
- the user arranges it in the OS’s display settings, not in an Ozma-only canvas.

An always-on-top window on the host would fail all of these. It is a mirror,
not a monitor, and must be described as such.

### EDID, modes, identity, and hotplug

- Generate EDID from the client’s panel data, intersected with what the
  provider/driver can actually do. Never advertise a mode the capture/encode
  path cannot sustain.
- Use a stable EDID serial and monitor name (`OZMA-<node>-<client>`) so the OS
  remembers position, scale, and per-display window placement across
  reconnects. Avoid duplicate identities for two clients with the same model.
- The OS is the source of truth for the selected mode. If the user changes it
  in display settings, follow the change, reconfigure the encoder, request a
  keyframe, and resize the client. Do not silently override it.
- HDR is offered only when the client panel, provider, capture path, encoder,
  and transport all support it. Otherwise stream SDR and show why.
- Rotation and scale are resolved once, host-side. The client reports its
  orientation; the host owns logical scaling so cursors and windows do not
  double-scale.
- Hotplug add/remove and DPMS changes are runtime events. They must not
  require a session restart or a node reboot.

### Input and cursor

The host OS already has a cursor and a topology. The remote display is one
rectangle in that topology; input must respect it.

- **Absolute pointer:** client sends coordinates relative to its surface; the
  input router applies the display’s offset, scale, and rotation, then injects
  a global absolute event (`uinput`/libei on Linux, `SendInput` on Windows,
  `CGEvent` on macOS).
- **Relative pointer / pointer lock:** games and 3D apps need raw relative
  motion. Capture it with pointer lock, send deltas, and do not apply host
  pointer acceleration twice.
- **Touch and pen:** carry the display id and map to that display; bind to the
  output where the compositor supports it, and preserve pressure/tilt.
- **Edge crossing:** the host compositor moves the cursor between the virtual
  display and local displays. The client sees the cursor leave/enter and must
  not keep injecting.
- **Focus and release:** focus loss, disconnect, DPMS, lock, or display removal
  triggers `release_all`; no stuck keys or buttons.
- **Multi-client:** only the session’s controller injects input. Viewers see
  the display but cannot move the pointer; control is explicitly handed off.

### UX

- **Add remote display:** from the host or client, choose a paired device. The
  display appears immediately in the host’s display settings; an optional
  compact “arrange” dialog can call the OS settings or write the topology, but
  the OS remains authoritative.
- **Default extend, not mirror.** The user can choose mirror/primary later in
  the normal display settings. Persist per-client layout.
- **Disconnect:** default hold the display for a short grace period (for
  example 30 seconds) so windows do not reflow while the network recovers;
  then remove it unless the user chose “keep this display when disconnected”
  (useful for headless hosts). Reconnecting with the same EDID restores the
  layout.
- **Client presentation:** windowed or full-screen; windowed mode is still a
  monitor, not a letterboxed video player. No host-side window.
- **DPMS:** client screen off / app backgrounded can put the virtual display
  into DPMS off and stop encoding; input or a wake request restores it.
- **Audio:** optionally make the remote device’s speakers the session’s audio
  sink; show which sink is active.
- **Quality:** damage/idle-aware encoding; if the link degrades, lower FPS or
  bitrate before changing the host mode (mode changes reflow windows).
- **Privacy:** a visible indicator on both ends, consent for attaching a
  display, `display_attach` permission, and an audit entry. A remote monitor is
  another place sensitive content can appear; treat it like one.

### API

```http
GET    /api/v1/displays
POST   /api/v1/displays                 # create/attach a remote display
GET    /api/v1/displays/{id}
PATCH  /api/v1/displays/{id}            # name, scale, rotation, privacy
DELETE /api/v1/displays/{id}            # detach, with hold/remove policy
POST   /api/v1/displays/{id}/mode
POST   /api/v1/displays/{id}/dpms
GET    /api/v1/displays/topology
PUT    /api/v1/displays/topology
```

Capability advertisement:

```json
{
  "virtual_display": {
    "supported": true,
    "backend": "evdi",
    "max_outputs": 4,
    "hotplug": true,
    "programmable_edid": true,
    "stable_identity": true,
    "hdr": false,
    "modes": ["2560x1440@120", "1920x1200@60", "1920x1080@60"]
  }
}
```

A session track can then be:

```json
{
  "kind": "virtual_display",
  "display_id": "ozma-node-a3f2/laptop-1",
  "mode": "2560x1440@120",
  "hdr": false,
  "input": "absolute",
  "audio": true
}
```

The Hestia adapter maps `virtual_display.enabled` and its requested
width/height/FPS/HDR into this provider, and reports the actual created
display rather than assuming it exists.

### Hardware KVM boundary

For a hardware node with no agent on the target, the target only sees a monitor
if the HDMI capture path presents a valid sink with EDID and HPD. That makes it
a real extra monitor to the target, but only within the capture hardware’s
limits:

- many USB capture dongles expose a fixed EDID/mode and cannot be changed;
  they are still an extra fixed monitor, not an arbitrary remote panel;
- programmable EDID/HPD capture hardware can advertise the client’s modes and
  behave much more like the software path;
- an agent on the target is always the better path for arbitrary modes, HDR,
  and no extra cable;
- the node cannot arrange the target OS’s display settings for the user; that
  remains a normal OS action on the target, which is the point of making it a
  real monitor.

### Acceptance criteria

- On Linux with EVDI/KMS, a remote laptop appears in the host’s display
  settings as `OZMA-…`, can be set to a supported mode, accepts a dragged
  window, runs a full-screen app, and appears in an ordinary screen capture.
- The host has no Ozma overlay window pretending to be a monitor.
- Absolute, relative, touch, and pen input land on the correct display at
  100%/125%/150% scale and 0°/90°/180°/270° rotation.
- Unplug → reconnect restores the same display identity and layout, or
  removes it cleanly after the grace period without losing windows.
- On unsupported platforms, the UI says “mirror”, not “additional monitor”.

---

## 11. Identity, pairing, and permissions

### Device identity

- Node has an Ed25519 identity key, ideally in a TPM/secure element where
  available; otherwise a protected file with clear migration.
- Enrollment with the controller’s mesh CA; WireGuard key is the data-plane
  identity; mTLS/QUIC certificate is the control-plane identity.
- Short-lived session tokens are signed by the controller and scoped to a
  session/client; no long-lived shared bearer token in a URL or log.

### User pairing

Replace the default 4-digit PIN with:

1. **Passkey/WebAuthn** when the client has a browser or platform
   authenticator.
2. **QR code** on the node console/HDMI/OLED or in the controller UI:
   `ozma://pair?node=<id>&nonce=<...>&fp=<...>`.
3. **Short code** (6 digits, rate-limited, single-use, expires in 60 s) only
   for constrained clients/consoles.
4. **SSH key** for headless Linux users; **PAM** for local OS accounts where
   the node is a softnode/agent, not as the default for an appliance.

Pairing creates a client identity and a set of permissions; it is not a
certificate-file workflow.

### Permissions

Scopes, not a single admin flag:

`view`, `control`, `audio`, `mic`, `clipboard_read`, `clipboard_write`,
`file_transfer`, `usb`, `power`, `commands`, `admin`.

- Per-client permission grants, revocable, with last-used and audit.
- Consent can grant a subset for one session (“view only”, “control for 1
  hour”).
- Clipboard and file transfer are off until explicitly granted; show an
  indicator when active.
- Local unlock can be configured to end the session or keep it, exactly as the
  fork’s “disconnect on unlock” does, but implemented as policy rather than a
  compositor patch.

---

## 12. UX blueprint: better than Hermes

Hermes’s weaknesses are recognizable: a large config file, compositor-specific
setup, virtual-display failures that look like black screens, a 4-digit PIN,
and a web UI that exposes implementation choices. The node should make the
following the default experience.

| Hermes pain | Ozma UX |
|---|---|
| “Add computer”, ports, PIN, certificate | One **Connect** action after discovery; passkey/QR pairing; no ports |
| Choose UDP/TCP/SSH/QUIC | Automatic path selection with a visible “Direct / Relay / SSH” badge |
| Black screen with audio | **Readiness card**: “Capturing 2560×1440@120, encoding H.264 (VAAPI), 12 ms” or a precise failure and fix |
| Config files for resolution/FPS/HDR | Client asks for a **profile**; node returns the closest supported mode and why |
| Multi-monitor as separate sessions | One session, multiple tracks, a monitor picker with layout preview |
| A laptop/tablet can only mirror a screen | **Add remote display** creates a real extra monitor where the platform supports it; otherwise it is clearly labelled “mirror”, never “additional monitor” |
| Second client steals input | Explicit **viewer / controller** roles and a “Request control” handoff |
| Disconnect ends everything | Session survives short disconnects; **Resume** is the default; explicit End |
| Physical monitor stays on | **Privacy policy** with local indicator, consent, lock/blank/DPMS where hardware supports it |
| No idea what went wrong | Live quality panel: RTT, jitter, loss, FEC, bitrate, FPS, dropped frames, encoder, temperature; one-click report |
| Update/restart surprises | Signed updates, maintenance window, rollback, post-update self-test |

### Flows

1. **First boot:** node advertises, shows a QR/URL on its local console or
   HDMI, LED indicates “ready to pair”. No IP address required.
2. **Pair:** client scans; node asks local confirmation only if policy
   requires; permissions are shown in plain language.
3. **Connect:** client picks “this node” or a saved desk layout; node runs
   preflight; first frame appears only after readiness passes.
4. **Add remote display:** from the host or client, choose a paired device. It
   appears in the host’s display settings as an `OZMA-…` monitor; arrange it
   with the OS’s normal tools. If no provider exists, the UI offers a labelled
   mirror instead.
5. **In session:** minimal overlay with quality, audio, privacy, and end.
   Settings are contextual; no modal configuration.
6. **Failure:** the video area becomes a diagnostic card with an action
   (“Replug HDMI”, “Use HLS fallback”, “Switch to software encoder”).
7. **Disconnect:** local display restored, remote display held briefly for
   reconnect or removed cleanly, session kept for resume, audit written.
8. **Multi-client:** second client sees “In use by Alice (view only)” and can
   request control.
9. **Handoff:** move the session to another client/device with a resume token
   or QR; input state is released, never stuck.

### Accessibility and local operation

- Keyboard-only navigation for the web UI; screen-reader labels.
- Local console/front-panel flow for pairing, IP display, reboot, factory
  reset, and “who is connected”.
- Works with no internet, no controller UI, and no desktop.

---

## 13. Hardware tiers and honest limits

Parity must be capability-gated. A Pi Zero cannot do what an N150 with VAAPI
does, and pretending otherwise produces the black-screen failures the fork is
trying to eliminate.

| Tier | Typical hardware | Realistic target |
|---|---|---|
| A | x86/ARM with VAAPI/V4L2M2M, USB 3 capture | 1080p60/1440p, H.264/HEVC, single or multi-track, optional HDR, low-latency RTP/QUIC |
| B | H618/RPi-class SBC, USB 2 capture | 1080p30 H.264 (hardware if available), single track, MJPEG fallback, LL-HLS/RTP, no HDR |
| C | MCU/Teensy | HID/audio only; video via a separate node/controller path |

The node must advertise:

- capture formats/resolutions/FPS and whether the source is compressed;
- encoder/codec/profile/rate-control capabilities and whether the probe passed;
- audio formats and whether mic/loopback exists;
- max simultaneous sessions/tracks and a resource budget;
- display/EDID/DDC/CEC capability;
- virtual-display providers/backends, max outputs, programmable EDID/HPD,
  HDR/rotation/scale support, and whether the result is a real OS monitor or
  only a compositor-managed output;
- USB gadget functions and USB/IP capability;
- supported transports and current measured quality.

Remote-display capacity is a separate budget from ordinary capture. Tier A can
drive several remote displays if the GPU/encoder and network allow; Tier B can
drive one modest remote display; Tier C should not advertise one. The provider
runs on the host, while the node still has to capture and encode the new
scanout, so both sides must fit the budget.

**Privacy caveat for hardware KVM:** a passive HDMI capture node cannot lock
the target OS or stop a local monitor that is wired directly to the target.
It can blank/sleep the local monitor only if it controls it (DDC/CI, CEC, a
loop-through/matrix, or a relay). If it cannot, the UI must say so and offer
target-side lock/agent options. This is better than claiming a privacy mode
that cannot be enforced.

---

## 14. API and compatibility contract

### Native node API

```
GET    /api/v1/node
GET    /api/v1/capabilities
GET    /api/v1/health
GET    /api/v1/diagnostics
GET    /api/v1/diagnostics/report
GET    /api/v1/metrics                 # Prometheus
GET    /api/v1/sessions
POST   /api/v1/sessions                # create/prepare, idempotency key
GET    /api/v1/sessions/{id}
POST   /api/v1/sessions/{id}/start
POST   /api/v1/sessions/{id}/stop
POST   /api/v1/sessions/{id}/resume
DELETE /api/v1/sessions/{id}
GET    /api/v1/sessions/{id}/events    # SSE or WS
GET    /api/v1/displays                # virtual/remote displays
POST   /api/v1/displays
GET    /api/v1/displays/{id}
PATCH  /api/v1/displays/{id}
DELETE /api/v1/displays/{id}
POST   /api/v1/displays/{id}/mode
POST   /api/v1/displays/{id}/dpms
GET    /api/v1/displays/topology
PUT    /api/v1/displays/topology
POST   /api/v1/pairing/offer
POST   /api/v1/pairing/complete
GET    /api/v1/clients
DELETE /api/v1/clients/{id}
GET    /api/v1/display
POST   /api/v1/display/privacy
GET    /api/v1/usb
POST   /api/v1/usb/...
```

Every error uses the same shape:

```json
{
  "ok": false,
  "error": {
    "code": "capture.no_frames",
    "message": "The capture device opened but produced no frames.",
    "details": { "device": "/dev/video0", "format": "MJPG" },
    "remediation": "Check the HDMI cable and that the source is powered on.",
    "retryable": true
  }
}
```

### Hestia adapter

Map the native session API onto the fork’s endpoints so the desktop client and
Hestia clients work unchanged:

- `GET /api/hestia/v1/capabilities`
- `POST /api/hestia/v1/session/prepare` (including `virtual_display.enabled`)
- `POST /api/hestia/v1/session/stop`
- `GET /api/hestia/v1/display/status`
- `POST /api/hestia/v1/display/recover`
- `GET /api/hestia/v1/client/permissions`
- `GET /api/hestia/v1/diagnostics`
- `GET/POST /api/hestia/v1/clipboard`
- `GET/POST /api/hestia/v1/commands/run`

### Moonlight/GameStream adapter

Only in `compatibility = "moonlight"` mode: HTTPS `/serverinfo`, `/pair`,
`/applist`, `/launch`, `/resume`, `/cancel`; RTSP `48010`; ENet control; RTP
video/audio/FEC; client-certificate pairing. This is an adapter, not the
native path, and it should be optional because it dictates ports and crypto.

### Controller compatibility

Keep the endpoints the current controller already proxies:

`/health`, `/usb`, `/power/*`, `/current`, `/rgb/*`, `/display/snapshot`,
`/display/info`, `/stream/*`, `/webrtc/offer`, `/webrtc/bitrate`,
`/input/key`, `/input/mouse`, `/input/type`, `/input/ws`, `/metrics`,
`/api/v1/connection`.

Add a versioned `/api/v1/*` surface and deprecate the old paths gradually.

### Desktop client fixes

The current `ozma-desktop-client` is written against a partly imagined
contract:

- It calls `/api/machines`, `/api/switch/{id}`, `/api/usb/route`; the
  controller exposes `/api/v1/nodes`, `/api/v1/nodes/{id}/activate`, and has no
  `/api/usb/route`.
- It spawns `hermes --h3-proxy` and `ssh hermes --proxy`; a node-native client
  should speak QUIC in-process and not require a Hermes binary.
- It expects Hermes ports (`47984`, `47989`, `47990`, `48010`, `48443`); the
  node should advertise its own and support a Hermes compatibility profile.
- The multi-monitor preview is a plan, not a decoder; the client needs real
  video decode/render (WebTransport + hardware decode or a platform decoder).

Define the shared `ozma-protocol` crate first, then make both the client and
the node consume it. That is cheaper than retrofitting two independent
contracts.

---

## 15. Phased plan

### M0 — Contract and cleanup (1–2 weeks)

- Add `ozma-protocol`: capabilities, session states, error codes, transport
  descriptors, version negotiation.
- Wire or delete the orphaned modules; make `ozma-node` build with `rustls`.
- Add `/api/v1/health`, `/capabilities`, `/sessions` skeleton, `/metrics`,
  OpenAPI/JSON schema.
- Fake `CaptureSource`, `Encoder`, and `Transport` for deterministic CI.
- Acceptance: node builds, starts, advertises, registers, serves the API, and
  a fake session reaches `Ready` in CI.

### M1 — One real media path (3–5 weeks)

- V4L2 capture with MMAP, hotplug, format negotiation, and frame validation.
- Probe a real frame and support H.264 with a software encoder fallback.
- Ship a single-track native QUIC session using the shared contract from M0.
  Carry control, input, and media on the authenticated connection.
- Require scoped `view` and `control` grants and enforce configured workstation
  consent before the corresponding media or input starts. Use controller-issued
  credentials for this milestone. Release held keys and buttons on disconnect
  or loss of control. Earlier unauthenticated bring-up stays in a local harness.
- Accept: the native desktop client discovers and activates the target through
  the controller, starts a 1080p30 LAN session, renders decoded video and plays
  audio, and sends keyboard and mouse events that visibly change the source.
  Verify that denied consent blocks session startup, view-only input is rejected,
  and disconnect releases held input. Measure input latency against the <30 ms target.

### M2 — Transport, auth, and UX (3–4 weeks)

- Extend native QUIC to browser WebTransport, add Unix/SSH fallback and path selection.
- Add passkey/QR/short-code pairing and PAM/SSH adapters.
- Add readiness probes, structured diagnostics, and live quality metrics.
- Keep controller proxy compatibility and add Hestia/RTSP compatibility over
  the same session core. Verify RTSP `SETUP`, `PLAY`, frame decode, and `TEARDOWN`
  separately from native startup.
- Prototype a single remote monitor on Linux with Xorg, the modesetting driver,
  EVDI, and libevdi pixel capture. Gate the MVP on a spike that verifies Xorg
  adopts the connector as an extended output and libevdi supplies its frames.
  Record the tested distro, kernel, and driver versions before claiming support.
- Accept: browser and native clients connect without port configuration and
  get actionable errors on authentication or transport failure. On the validated
  Linux configuration, the host sees the extra display in its settings, a
  window dragged onto it appears in the remote client, and pointer input lands
  on that display. Unsupported configurations offer mirroring.

### M3 — Multi-track, sessions, privacy (3–4 weeks)

- Multiple capture sources/tracks; per-track encoders; SDP extensions;
  per-track absolute input mapping.
- Multi-display remote monitors: topology, hotplug, stable EDID identity,
  per-display input mapping, hold-on-disconnect/reconnect.
- Viewer/controller roles, handoff, resume, session list/terminate.
- Expand privacy controls and consent UX; softnode display backends; hardware DDC/CEC
  where available.
- Clipboard channel with explicit permissions.
- Accept: two monitors/tracks, second client as viewer, reconnect after
  network drop, local unlock policy works, remote display restores its layout.

### M4 — Platform and hardware depth (3–6 weeks)

- HDR and DMABUF zero-copy where capable; HDR remote display where the panel,
  provider, capture, encoder, and transport all support it.
- Windows IddCx provider, best-effort macOS `CGVirtualDisplay` provider,
  compositor-native Linux providers, and the hardware HDMI-sink/EDID path.
- USB/IP server, virtual media, gadget composition, power/RGB/sensors parity
  with the Python node.
- Softnode backends: KMS/PipeWire/portal capture, KWin/Mutter/wlroots privacy,
  app/command providers.
- Recording, snapshot, report bundle.
- Accept: hardware node feature parity with `node/node.py`; softnode parity
  where the compositor allows.

### M5 — Hardening and parity closure (continuous)

- Fuzz RTSP/QUIC/auth/SDP/input parsers; soak tests; thermal/power limits.
- Security review, least-privilege packaging, reproducible builds.
- OTA/A-B updates, local console, localization, accessibility.
- Moonlight compatibility mode and client certification.

---

## 16. Testing strategy

- **Unit:** codec/SDP/RTP parsers, state machine transitions, auth token
  scopes, input mapping, resource budgets.
- **Property:** session state machine never reaches an invalid state; resume
  tokens expire; input release always clears all keys/buttons.
- **Fuzz:** RTSP headers/SDP, QUIC/HTTP3 frames if custom, pairing payloads,
  clipboard MIME, USB descriptors.
- **Golden:** SDP for one and N tracks; RTP packetisation for keyframes;
  EDID/CVT mode generation; error JSON shapes.
- **Integration:** fake source/encoder/transport; real V4L2 loopback; two
  clients; network impairment (loss/jitter/reorder); Wi-Fi roam/QUIC
  migration; relay/SSH/Unix paths; remote-display attach/detach, mode change,
  DPMS, scale/rotation input mapping.
- **Virtual display:** fake provider for deterministic topology/EDID tests;
  real EVDI/KMS and IddCx smoke tests; verify the display appears in the OS’s
  own settings and that ordinary screen capture can see it.
- **Hardware:** at least one Tier A and one Tier B node; capture card matrix;
  programmable-EDID/HPD sinks; thermal soak; suspend/resume; HDMI hotplug.
- **Security:** auth bypass, token replay, CSRF/origin, path traversal, SSRF,
  privilege boundaries, no secrets in logs.
- **UX:** first-run pairing without docs; failure cards actionable; remote
  monitor appears as a monitor, not a window; keyboard only; screen reader
  labels.

SLOs worth enforcing in CI/manual tests:

- session start < 2 s on LAN; first frame < 1 s after readiness;
- input-to-photon < 50 ms on wired LAN, < 100 ms on good Wi-Fi;
- reconnect < 1 s after a brief drop;
- remote-display attach < 2 s; mode change < 1 s; reconnect restores the same
  monitor identity and layout;
- no orphan encoder/capture/virtual-display processes after 100 start/stop
  cycles;
- zero stuck keys/buttons after focus loss or disconnect.

---

## 17. Risks and decisions to make early

1. **Codec backend.** Pure-Rust encoders are not yet a realistic path for
   low-latency hardware encode on all tiers. Recommended: a typed backend
   using `ffmpeg-next`/GStreamer for production codecs, pure-Rust for tests and
   simple software fallback. Decide before M1.
2. **Native transport choice.** QUIC/WebTransport gives one-port, browser-
   friendly, migratable transport; raw RTP/UDP is simpler and lowest latency on
   LAN. Support both behind one channel model; do not make QUIC mandatory for
   Moonlight compatibility.
3. **Moonlight scope.** Full GameStream compatibility is a large compatibility
   project. Keep it an adapter and prioritize the first-party client/Hestia
   path first; add Moonlight when a real client must be supported.
4. **Controller in the media path.** Direct is better on LAN, but some
   deployments require controller relay/NAT traversal. Make direct the
   default and relay a measured fallback, not the architecture.
5. **Hardware privacy.** A passive capture node cannot enforce target-side
   privacy. Design the hardware/loop-through/DDC/CEC path explicitly, or state
   the limitation in the UI.
6. **Resource limits on SBCs.** Multi-track + software encode + HDR can
   overcommit. Make per-session budgets and graceful degradation part of the
   protocol from day one.
7. **Security boundary.** Gadget/DRM/display helpers need privilege; keep them
   small, auditable, and IPC-only. Do not run the network API as root.
8. **Dependency portability.** Standardize on `rustls`; avoid native OpenSSL,
   libssh, and PAM linkage in the default build where optional features can
   provide them.
9. **Contract churn.** The desktop client and controller already disagree on
   endpoint names. Freeze `ozma-protocol` and generate client/controller types
   from it before building more UI.
10. **Scope creep.** “Parity with Hermes” can mean everything from HDR10 to
    custom KWin patches. The parity target should be the user outcome:
    low-latency remote access with input/audio/multi-monitor/privacy that works
    on the supported hardware, not a feature-count contest.
11. **Virtual-display driver dependency.** EVDI is an out-of-tree kernel
    module; Windows needs a signed IddCx driver; macOS relies on a private API.
    Pick a support matrix and make the fallback visible. Do not make the whole
    product depend on one kernel driver.
12. **“Normal monitor” is a promise.** A compositor-native output may not
    appear in every display tool; a window is never an extra monitor. Capability
    advertisement and UI wording must match what the platform actually
    provides.
13. **Remote-display edge cases.** Mode changes reflow windows; disconnect
    can lose window placement; rotation/scale cursor mapping is easy to get
    wrong; an extra display is another place sensitive content can appear. Test
    these explicitly and make the privacy/consent model cover displays, not
    just streams.

---

## 18. Immediate next tasks

1. Make `ozma-node` build with `rustls`; remove or feature-gate native-TLS.
2. Decide which orphaned modules are real: wire `v4l_enum`, `capture`,
   `serve`, `hotplug`, and `tasks` into a supervisor or delete them.
3. Write `ozma-protocol` v0: capabilities, session states, errors, transport
   descriptors.
4. Add a fake capture source/encoder/transport and a CI test that reaches
   `Streaming`.
5. Replace the static `/health` with `/api/v1/health`, `/capabilities`,
   `/sessions`, `/diagnostics`, `/metrics`.
6. Implement V4L2 capture + a real frame probe; report the first frame.
7. Implement H.264 framing over native QUIC and verify one decoded frame in the desktop client.
8. Implement HID gadget writes and a safe `release_all` path.
9. Enforce scoped session tokens and configured consent before enabling interactive sessions. Add passkey/QR pairing in M2.
10. Define the controller/client contract in one crate and update the desktop
    client to stop spawning `hermes` and to use the real controller endpoints.
11. Prototype the remote-display path: a fake `VirtualDisplayProvider` in CI,
    then the Xorg/EVDI/libevdi configuration from M2 and a native client window. Verify the
    display appears in `xrandr`/display settings, accepts a window, and maps
    absolute input at 100% and 150% scale.

Use local fixtures for early media bring-up. The first interactive network
release includes authentication, authorization, and configured consent.
Compatibility adapters follow the native path.
