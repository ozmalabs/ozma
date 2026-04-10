# Ozma Display — Universal Display Client

> A display client is not a remote desktop viewer. It is a first-class Ozma
> endpoint that consumes, produces, and routes content — with or without a
> controller, with or without an agent on the host machine.

---

## What it is

The existing Ozma stack has a clear producer side: nodes and agents capture
keyboard, mouse, video, and audio from target machines and route it through
the controller. What is missing is a **dedicated consumer** — an application
whose job is to receive and present that content in a coherent, configurable,
multi-source display environment.

**Ozma Display** fills this role. It is a native application (Windows, Linux,
embedded Linux) that:

- Connects to an Ozma mesh via WireGuard (same enrollment model as everything
  else — QR or token, one-time, hardware-backed where available)
- Consumes streams from any source the controller knows about — nodes, agents,
  cameras, media servers, dashboards
- Routes keyboard and mouse input from its own connected peripherals back
  through the mesh to the controller (reverse HID)
- Optionally runs as a full Wayland compositor on Linux, replacing the host
  desktop entirely — **OzmaOS mode**
- Optionally runs as a minimal framebuffer process on embedded hardware with no
  OS desktop at all

No agent is required on the machine running Ozma Display. The agent is an
enhancement that enables bidirectional content exchange between the display
machine and the mesh.

---

## The three modes

### 1. App mode (Windows and Linux)

A native windowed application. Multiple stream windows, each showing one
source. Keyboard and mouse events can optionally be routed back to the
controller. Looks and behaves like any other app on the host desktop.

Use cases: a laptop that wants to view a homelab server's output while still
running its own desktop; a secondary monitor that shows a camera feed and a
remote machine's terminal; an always-on dashboard display on a spare monitor.

### 2. OzmaOS mode (Linux compositor)

A Wayland compositor that IS the desktop. No host window manager runs beneath
it. Each visible source gets a compositor surface. Native Wayland apps
(terminals, browsers, tools) can run alongside remote streams — they are just
surfaces like any other.

The result: you sit down at a machine running OzmaOS, and all of your remote
machines are immediately visible. Edge crossing works exactly as it does on the
controller — move the cursor to the right edge of Machine A's surface, and it
enters Machine B. Focus follows the cursor.

OzmaOS is not a Linux distribution. It is a compositor + Ozma Display shell
that runs on top of any Linux kernel. It can also ship as a prebuilt image
(analogous to the node SBC image) for machines that should boot directly into
this experience.

### 3. Embedded / remote display mode

A headless display process for systems with no GPU desktop stack. Renders
directly to framebuffer or a hardware-accelerated DRM device. Targets:
Raspberry Pi 4/5, Intel NUC, any SBC with a display output.

The primary use case is an **Ozma Display Stick** — a Raspberry Pi or similar
attached to any TV or monitor via HDMI, enrolled in the mesh, and presenting
whatever the controller tells it to show. It is the software equivalent of a
smart TV stick, but for your own infrastructure.

---

## Sources

Ozma Display can consume any of the following, selectable in any combination:

| Source type | Protocol | Requires |
|---|---|---|
| Ozma node (hardware or soft) | HLS / H.265 RTP → HLS | Controller |
| Ozma agent (desktop capture) | WebRTC or HLS | Agent on target machine |
| Ozma camera node | HLS / RTSP | Camera node enrolled in mesh |
| Frigate camera stream | HLS | Frigate + camera node |
| Ozma screen widgets / dashboard | WebSocket (Tier 2 native render) | Controller |
| Jellyfin / media server | HLS, DASH | Jellyfin reachable on mesh |
| Sunshine game stream | Moonlight protocol | Sunshine on target |
| Local desktop / screen capture | PipeWire capture (Linux) / DXGI (Windows) | None |
| RTSP source (IP camera, DVR) | RTSP → decode | Network access |
| WebRTC peer (direct, no relay) | WebRTC | Both endpoints reachable |
| Static / browser-based content | WebView embed | None |

Sources are registered with the controller and made available to all display
clients. A display client subscribes to whichever sources it wants to show.
Layout configuration (which source on which display, at what position and
size) is stored on the controller and pushed to the display client on connect.

---

## Reverse HID

The display client has peripherals — keyboard, mouse, possibly a touchscreen.
Rather than consuming those peripherals for the local OS (or having them
silently ignored), Ozma Display routes them back to the controller via the
WireGuard tunnel.

From the controller's perspective, a display client is an **input source** —
the same conceptually as a physical keyboard/mouse plugged into the controller.
The controller routes this input to whichever node is currently active. Switch
the active node, and keyboard/mouse input starts going to a different machine.

This closes the loop: the display client consumes video/audio from machines
while simultaneously acting as the keyboard/mouse provider to those same
machines. The hardware is:

```
Display machine:
  ┌─────────────────────────────────────┐
  │  keyboard + mouse                   │
  │       ↓ (local USB)                 │
  │  Ozma Display                       │
  │       ↓ (WireGuard, port 7380)      │
  │  Controller ← input source          │
  │       ↓ (UDP, port 7331)            │
  │  Active node → target machine USB   │
  └─────────────────────────────────────┘
  │  Video/audio ←──────────────────────┘
  │  (HLS / WebRTC / VBAN)
  └──── displayed in compositor or window
```

A display machine running OzmaOS is therefore a complete zero-cable KVM
console: plug in your keyboard, mouse, and monitor; everything else is
software over the mesh.

---

## Agent integration

The agent is not required, but when it runs on the same machine as the display
client, the two should communicate directly over localhost (Unix socket or
localhost TCP) to avoid a round-trip through the controller for local
coordination.

| Capability | Without agent | With agent |
|---|---|---|
| Consume remote streams | Yes | Yes |
| Route input to remote nodes | Yes | Yes |
| Share display machine's own screen into the mesh | No | Yes |
| Clipboard synchronisation | No | Yes |
| Resolution and display topology awareness | No | Yes |
| Audio from display machine into mesh | No | Yes |
| Presence detection (lock screen, meetings) | No | Yes |

When an agent is running, Ozma Display registers the display machine as a
source in the controller — other display clients or the controller dashboard
can then consume its screen output, exactly as they would consume any other
node. The display machine becomes both consumer and producer simultaneously.

The local channel between display client and agent:

```
Ozma Display ←──── Unix socket / localhost:7392 ────→ ozma-agent
     |                                                       |
  renders local screen                              provides window list,
  as a source in mesh                               clipboard, resolution,
                                                    audio graph
```

The controller is not involved in this local channel. State that matters to the
rest of the mesh is synced to the controller by the agent separately.

---

## OzmaOS compositor — architecture

OzmaOS is a Wayland compositor built on [Smithay](https://github.com/Smithay/smithay)
(Rust). It manages surfaces of two kinds:

**Remote surfaces** — Wayland surfaces whose pixel content is decoded video
from an Ozma stream. To native Wayland apps running inside the compositor,
these look like ordinary windows. To the compositor, they are FFmpeg/GStreamer
decode pipelines writing into shared DRM buffers.

**Local surfaces** — ordinary Wayland windows from apps launched inside the
compositor. A terminal, a browser, a settings panel. These run as XWayland or
native Wayland clients.

The compositor shell (the equivalent of a taskbar/workspace manager) is
purpose-built for Ozma:

- No traditional taskbar or app launcher — the workspace IS the multi-machine
  view
- Each monitor can be assigned to one source, or split into a configurable
  tiled layout
- Source assignment is pushed from the controller (or overridden locally)
- Edge crossing is implemented natively at the compositor level — cursor motion
  past a surface edge triggers a HID routing switch on the controller

### Workspace layouts

```
Layout: Single focus (one machine full-screen)
┌──────────────────────────────┐
│                              │
│         Machine A            │
│         (full monitor)       │
│                              │
└──────────────────────────────┘

Layout: Picture-in-picture
┌──────────────────────────────┐
│                              │
│         Machine A            │       ┌──────────┐
│                              │  +    │ Cam feed │
└──────────────────────────────┘       └──────────┘

Layout: Tiled 3-up (PLP or ultrawide)
┌──────────┬──────────────────┬──────────┐
│          │                  │          │
│  Machine │    Machine B     │  Machine │
│    A     │  (primary/wide)  │    C     │
│          │                  │          │
└──────────┴──────────────────┴──────────┘
```

Layouts are stored as named presets on the controller and can be pushed to
display clients as part of scenario switching — when you switch to "gaming"
mode, the layout changes to single-focus on the gaming machine; when you
switch to "work" mode, it tiles the three work machines.

### Input routing in OzmaOS

The compositor receives keyboard and mouse events from local Wayland clients
AND from the physical peripherals of the display machine. All physical input
is forwarded to the controller as reverse HID. Focus determines which node the
controller routes it to:

- Cursor enters Machine A surface → controller switches active node to A
- User presses a hotkey → controller handles it (switch node, change layout, etc.)
- Cursor enters a local Wayland window → input goes to local app, not to mesh

This means the Ozma hotkey layer lives in the compositor, not in an evdev
intercept daemon. The hotkey is intercepted before it reaches any surface.

---

## Embedded / remote display image

The embedded image is an Alpine or Buildroot-based Linux system that boots
directly into Ozma Display (framebuffer/DRM mode) with no desktop environment.
The image is analogous to the existing hardware node images.

Target hardware at launch:
- Raspberry Pi 4 / 5 (HDMI, USB peripherals, good GPU decode)
- Generic x86 mini-PC (N100-class Intel, HDMI, DisplayPort)
- Any SBC with mainline kernel + HDMI + hardware H.264 decode

The image includes:
- WireGuard (kernel module + wg-quick)
- FFmpeg with hardware decode for the target SoC
- Ozma Display (statically linked where possible)
- Enrollment tooling (QR-code over HDMI on first boot)
- OTA update support (same A/B partition + Ed25519 signature model as nodes)

**First-boot enrollment:** On first boot with no configuration, the display
shows a QR code containing the enrollment token request. The user scans it
with the Ozma mobile app. The controller approves, pushes WireGuard config,
and the display is live. Total time: under 60 seconds from power-on.

The embedded display can also function as an **input-free media display** —
no keyboard or mouse needed. The controller pushes layout and sources remotely.
Suitable for lobby displays, meeting room dashboards, camera monitoring walls.

---

## Security model

Ozma Display enrolls identically to any other Ozma endpoint:

1. Display generates a WireGuard keypair
2. Controller issues an enrollment token (QR code, URL, or NFC tap on mobile)
3. On enrollment: WireGuard peer approved, device assurance level assigned
   - Linux/Windows with TPM: level 2 (hardware-bound key)
   - Raspberry Pi / embedded: level 1 (software-protected, fw-signed OTA)
4. All mesh traffic is WireGuard encrypted end-to-end
5. Reverse HID traffic (keyboard/mouse back to controller) is inside the
   WireGuard tunnel — not a separate channel, not unauthenticated

**Display clients are not nodes.** They receive streams but are not USB HID
gadgets attached to target machines. They occupy a new device class
(`machine_class="display"`) with its own trust profile:

- `machine_class="display"` — has local peripherals, routes input to controller,
  consumes streams. Treated as an operator console, not a node.
- Agent action approval: inherits workstation policy (notify on mutating actions)
  if a local agent is present.
- Can be shared (another user can push their layout to this display), subject to
  the display owner's consent.

Display clients are allocated addresses in **10.203.0.0/16** — separate from
nodes (10.200.x.x), mobile clients (10.202.x.x), and controller peering
(10.201.x.x). This ensures the `is_wireguard_source()` bypass is not inherited.

---

## Implementation

### Technology choices

| Component | Technology | Rationale |
|---|---|---|
| Core application | Rust + [Tauri](https://tauri.app) | Native performance, webview UI reuse, single binary, cross-platform |
| OzmaOS compositor | Rust + [Smithay](https://github.com/Smithay/smithay) | Wayland compositor library, production-ready, Rust-native |
| Video decode | GStreamer (Linux) / MediaFoundation (Windows) | Hardware decode on all targets; FFmpeg fallback |
| Audio output | PipeWire / ALSA (Linux), WASAPI (Windows) | Consistent with rest of Ozma on Linux |
| WireGuard | wireguard-rs (boringtun) embedded | Single binary, no kernel module required for app mode |
| UI | Existing Ozma dashboard (webview / React) | Reuse; controller's dashboard is already the source of truth |
| Embedded image | Alpine Linux + custom image build | Same toolchain as node images |

The Tauri backend handles all networking, WireGuard, decode pipeline, and
HID routing. The webview provides the configuration and layout UI — the
same component tree as the controller dashboard, communicating with the
display client's local API rather than the remote controller API directly.

### Phased delivery

**Phase 1 — App mode, stream consumption only**
- Windows and Linux (X11 and Wayland)
- WireGuard enrollment
- Connect to controller, enumerate sources
- Display one or more HLS streams in resizable windows
- No reverse HID in this phase

**Phase 2 — Reverse HID + audio**
- Keyboard/mouse events forwarded to controller via WireGuard
- Controller routes them to active node (existing infrastructure)
- Audio from node routed back to display machine's output
- Layout persisted on controller, pushed on connect

**Phase 3 — OzmaOS compositor**
- Smithay-based Wayland compositor
- Remote surfaces as native compositor surfaces
- Edge crossing at compositor level
- Local Wayland apps running alongside remote streams
- Layout scenarios synced from controller

**Phase 4 — Embedded image + Display Stick**
- Alpine-based image build
- First-boot QR enrollment on HDMI
- OTA update support
- Raspberry Pi 4/5 hardware acceleration

**Phase 5 — Agent integration + bidirectional**
- Local socket between display client and agent
- Display machine's screen published as a source
- Clipboard sync
- Presence awareness (lock screen follows meeting state)

---

## How this fits the product

The existing Ozma product is primarily about **routing** — moving input between
machines. The display client adds the **consumption** side that was previously
handled ad-hoc (browser tab to watch a stream, VNC client to control a machine,
etc.).

Together they form a complete picture:

```
[Ozma node] → captures USB HID output → Controller
[Controller] → routes HID input → [Ozma node] → target machine USB

[Ozma Display] → reverse HID → Controller → [Ozma node] → target machine
[target machine] → video/audio → [Ozma node] → [Ozma Display]
```

The display client is what turns Ozma into an experience — not just a routing
fabric, but the actual surface through which you interact with all your machines.

For the Kickstarter, the **Ozma Display Stick** (Pi-based embedded image) is a
tangible hardware product in the same tier as the node hardware: plug it into
your TV, scan a QR code, instantly have a console for every machine in your
mesh. No cables. No KVM switch. No separate remote desktop app for each machine.
Every machine is a window.
