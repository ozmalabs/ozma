# System Architecture

## Overview

Ozma is a software-defined USB and AV fabric. A **Controller** (any Linux machine on the local network) manages a set of **Nodes** (small SBCs, MCUs, or software agents) that each attach to a target PC via USB. The target PC sees the node as ordinary USB peripherals — keyboard, mouse, audio device, and camera — while all routing decisions live in the Controller.

This inversion of the traditional KVM model (hardware switch in the signal path) means:

- No signal interruption when switching — the display stays connected and the monitor never resyncs.
- No host software required on the target — the node looks like a standard USB device class.
- Audio, camera, and video routes are first-class, not afterthoughts.
- The same hardware platform can run entirely different behaviors via software profiles (KVM, conference room, digital signage, lecture capture, live production, etc.).

---

## Three-Layer Model

```
┌──────────────────────────────────────────────────────────────┐
│  CONTROLLER  (Linux)                                         │
│                                                              │
│  ┌──────────┐  ┌───────────┐  ┌────────────┐  ┌─────────┐  │
│  │ REST API  │  │ WebSocket │  │  PipeWire  │  │   OBS   │  │
│  │  :7380   │  │   :7380   │  │   (audio)  │  │ (video) │  │
│  └──────────┘  └───────────┘  └────────────┘  └─────────┘  │
│         │              │              │               │      │
│         └──────────────┴──────────────┴───────────────┘      │
│                              │                               │
│                    Internal routing bus                      │
└──────────────────────────────┬───────────────────────────────┘
                               │
         ┌─────────────────────┼─────────────────────┐
         │                     │                     │
┌────────┴───────┐   ┌─────────┴──────┐   ┌─────────┴──────┐
│  Compute Node  │   │  Compute Node  │   │   Room Mic     │
│  (SBC / MCU)   │   │   (Soft Node)  │   │                │
└────────┬───────┘   └─────────┬──────┘   └────────────────┘
         │  USB-C               │  TCP/IP
         │                      │
┌────────┴───────┐   ┌─────────┴──────┐
│  Target PC A   │   │  Target PC B   │
│  (sees: HID,   │   │  (any desktop) │
│   UAC, UVC)    │   │                │
└────────────────┘   └────────────────┘
```

---

## Components

### Controller

Any Linux computer. Hosts:

- **REST API + WebSocket** (port 7380) — primary control plane for UIs, scripts, and node command push.
- **PipeWire** — audio session manager; routes microphone, speaker, and recording streams between nodes and local applications.
- **OBS** (optional) — video mixing; composites camera feeds and screen captures for recording or streaming scenarios.
- **mDNS listener** — discovers nodes advertising `_ozma._udp.local`, builds the node inventory.

The Controller does not sit in the USB signal path. It sends HID, audio, and video payloads over the LAN to the currently-active node.

### Compute Nodes

Small Linux SBCs or bare-metal MCUs. Supported platforms:

| Platform | Architecture | USB gadget support |
|---|---|---|
| Milk-V Duo S | RISC-V Linux | HID + UAC + UVC (configfs) |
| Raspberry Pi Zero 2 W | ARM Linux | HID + UAC + UVC (configfs) |
| Teensy 4.1 | Cortex-M7 bare-metal | HID + UAC (Arduino USB stack) |

Each node:

1. Presents USB gadget interfaces to its target PC via a USB-C cable (the target sees standard device classes).
2. Listens on UDP port 7331 for HID reports from the Controller.
3. Writes received HID reports directly to `/dev/hidg0` (keyboard) and `/dev/hidg1` (mouse).
4. Participates in audio routing via VBAN (port 6980) or Opus RTP (port 7340).
5. Optionally streams camera video via MJPEG (port 7332) as a UVC gadget source.

### Soft Nodes

Soft nodes emulate a hardware node using a QEMU VM. Instead of writing to `/dev/hidg0`, HID input is forwarded to the VM via QMP (QEMU Machine Protocol). Used for testing without hardware and for managing VMs as KVM targets.

### Agents

Agents run **inside** the target machine's OS (not on the node). They provide:

- Clipboard sync, display geometry, resolution changes
- Screen capture (for AI agent control)
- UI accessibility tree (for precise element targeting)
- Metrics collection (CPU, GPU, RAM, temps)

Agents are optional. The node works without one.

---

## Data Paths

### HID (keyboard + mouse)

```
evdev capture (Controller)
    → UDP packet (8-byte HID report)
    → Active node (port 7331)
    → /dev/hidg0 or /dev/hidg1
    → Target PC USB stack
```

Switching changes which node receives packets. The USB connection never moves.

### Audio

Two transport mechanisms:

- **PipeWire (local):** For soft nodes on the same host. `pw-link` connects the active node's audio source to the output sink. Switch latency ~8ms.
- **VBAN (network):** For hardware nodes. UDP audio at 48kHz/16-bit stereo, ~188 frames/sec.

### Video

- **VNC → HLS/MJPEG:** Soft nodes expose VNC; the controller transcodes to HLS for dashboard previews.
- **HDMI capture:** Hardware nodes with capture cards provide V4L2 → ffmpeg → HLS/MJPEG.
- **RTP H.265:** Video nodes stream directly over the mesh.

---

## Scenario Model

A **scenario** binds a name, colour, and node together. Switching scenarios changes the active node (and therefore which machine receives input, which audio source plays, which video stream is shown).

```json
{
  "id": "workstation",
  "name": "Workstation",
  "node_id": "ozma-node-a3f2._ozma._udp.local.",
  "color": "#4A90D9"
}
```

Scenarios are stored in `controller/scenarios.json` and managed via the REST API.

---

## Users and Zones

**Users** are the identity layer above Controllers. Each user owns a **zone** — a collection of controllers, nodes, and registered services.

```
User (alice)
  └── Zone ("Alice's Home")
       ├── Controller (home office)
       │    ├── Node A → Workstation PC
       │    └── Node B → Gaming PC
       └── Controller (study)
            └── Node C → Laptop dock
```

- One user can own multiple controllers (home, office, holiday house).
- Multiple users can link controllers into a **local mesh** (housemates on the same LAN).
- Each user has explicit ownership of their devices — "share everything" is a policy preset, not a special case.

Users are managed via `controller/users.json` and the REST API (`/api/v1/users`).

---

## Service Proxy

The Controller acts as a **reverse proxy** for internal services (Jellyfin, Gitea, Immich, etc.). Services are registered with a subdomain and the proxy routes requests based on the `Host` header.

```
jellyfin.alice.c.ozma.dev
    → DNS (Connect)
    → Controller (port 443, wildcard cert)
    → Reverse proxy to 192.168.1.50:8096
```

- **Connect subdomain**: each user gets `*.username.c.ozma.dev` with a wildcard Let's Encrypt certificate.
- **DNS-01 challenge**: coordinated via Connect (it controls the `ozma.dev` DNS zone). The TLS private key never leaves the controller.
- **Without Connect**: works as a plain HTTP proxy on the LAN using `*.localhost` matching.
- **Health monitoring**: background checks every 30 seconds per service.

Registered services are stored in `controller/services.json`.

---

## Identity Provider

The Controller runs a built-in **OIDC-compatible identity provider** that centralises authentication for the entire household or organisation.

- **Password login** for local users
- **Social login** (Google, Apple, GitHub) via OAuth2
- **Enterprise federation** with AD/Entra/LDAP
- **Session management** via httponly cookies
- **OIDC provider for LAN services** — Gitea, Nextcloud, etc. can point at `/.well-known/openid-configuration` for SSO

The IdP gates: the dashboard, proxied services (via session cookie), cross-user sharing, and API access (OIDC tokens alongside existing Ed25519 JWTs).

---

## Sharing

Users on linked controllers can share resources with explicit grants:

```
Alice shares her Jellyfin with Bob:
  bobsjellyfin.alice.c.ozma.dev
      → Alice's controller (authenticated via IdP)
      → WireGuard tunnel (LAN) or Connect relay (internet)
      → Bob's controller
      → Bob's Jellyfin
```

- **Share grants** specify grantor, grantee, resource type (service, node, audio, display), permissions, and optional expiry.
- **Local mesh**: controllers discover each other via mDNS (`_ozma-ctrl._tcp.local.`) and pair via the existing mesh CA.
- **Connect relay**: same mechanism but tunnelled through the Connect relay infrastructure for cross-internet sharing.
- Entry point is always the grantee's own domain — the tunnel chain is transparent.

---

## External Publishing

Services can be published to the internet under `.e.` subdomains:

| Domain pattern | Meaning |
|---|---|
| `jellyfin.alice.c.ozma.dev` | Internal — Connect mesh only |
| `jellyfin.alice.e.ozma.dev` | External — internet-accessible |

Two modes:
- **Private**: authenticated via the user's IdP — access your own services from outside.
- **Public**: open to anyone (requires admin confirmation). For blogs, public media servers, etc.

External publishing requires Ozma Connect for DNS and relay infrastructure.
