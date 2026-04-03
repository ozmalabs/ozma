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
         ┌─────────────────────┼──────────────────────────┐
         │                     │                          │
┌────────┴───────┐   ┌─────────┴──────┐   ┌──────────────┴─────┐
│  Compute Node  │   │  Compute Node  │   │   Camera Node       │
│  (SBC / MCU)   │   │   (Soft Node)  │   │  (Ozma Camera /     │
└────────┬───────┘   └─────────┬──────┘   │   NVR / DIY OPi5)  │
         │  USB-C               │  TCP/IP  └──────────┬──────────┘
         │                      │                     │  TCP/IP
┌────────┴───────┐   ┌─────────┴──────┐   ┌──────────┴──────────┐
│  Target PC A   │   │  Target PC B   │   │  IP Cameras (PoE)   │
│  (sees: HID,   │   │  (any desktop) │   │  (Frigate runs      │
│   UAC, UVC)    │   │                │   │   on the node)      │
└────────────────┘   └────────────────┘   └─────────────────────┘
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

### Camera Nodes

Camera nodes are a distinct node type (`machine_class: "camera"`) that produce video and handle local inference rather than consuming HID. The KVMA fabric treats them as first-class nodes — they register via mDNS, authenticate with the mesh CA, and appear in the controller's node inventory alongside compute nodes.

**What camera nodes do:**
- Run Frigate locally — all decode and ML inference happen on the node, not the controller
- Expose camera streams (RTSP, HLS/MJPEG) via their registration metadata
- Forward Frigate events (doorbell press, person detected, face recognised) to the controller's AlertManager
- Participate in two-way audio (camera mic → node headset; node mic → camera RTSP backchannel)

**What camera nodes do not do:**
- No USB gadget (no HID, no UAC — `capabilities: []` for keyboard and mouse)
- No target PC — they produce video output rather than serving a workstation

**Controller CPU impact is flat.** The controller only receives event metadata, snapshot JPEGs on demand, and stream URLs. No raw video is decoded on the controller regardless of camera count. Adding cameras means adding camera node hardware; the controller's load does not grow.

**Zero-config camera auto-detection.** When a camera is plugged into an Ozma NVR's PoE port, the NVR's auto-configuration pipeline handles everything:

1. Camera powers up and gets an IP from the NVR's built-in DHCP server (dedicated PoE subnet)
2. ONVIF probe — `WS-Discovery` confirms it's a camera; `GetProfiles` and `GetStreamUri` retrieve all stream URLs and their resolution/codec/framerate; `GetCapabilities` detects audio, PTZ, and doorbell button support
3. Frigate config is generated automatically: record stream set to the highest quality profile, detect stream set to a low-resolution sub-stream for efficient inference, RK3588 NPU detector pre-configured
4. If the camera has a doorbell button or two-way audio, those are wired to the Ozma alert system automatically
5. The camera appears in the controller dashboard — typically within 30 seconds of being plugged in

For cameras that do not support ONVIF, the pipeline falls back to known manufacturer RTSP URL patterns and `ffprobe` stream detection. No YAML editing is ever required.

**DoorbellManager auto-discovery:** When camera nodes are registered, `DoorbellManager` reads `camera_streams` from `NodeInfo` to resolve RTSP and backchannel URLs automatically. Manual `OZMA_DOORBELL_CAMERAS` configuration is only needed for third-party cameras that are not managed by Ozma.

**Deploy anywhere — including remote locations.** Camera nodes register with Ozma Connect independently, the same way compute nodes do. This means a camera node establishes its own WireGuard relay tunnel and appears in the controller's node inventory regardless of whether it is on the same LAN. A camera at a holiday home, a parent's house, a rental property, or a workshop with its own 4G router shows up in your controller exactly like a camera in the next room — same node inventory, same dashboard, same scenario switching, same two-way audio. No VPN to configure, no port forwarding, no manual network plumbing. Plug in, enroll with Connect, it's in the mesh.

**Network placement — normal LAN, not IoT VLAN.** Third-party IP cameras are untrusted firmware and should be isolated on an IoT VLAN where only Frigate can reach them. Ozma Camera nodes are different: they hold a mesh CA certificate, authenticate mutually with the controller, and carry all traffic over XChaCha20-Poly1305 encrypted transport. They belong on the normal network (or the mesh overlay) — no VLAN configuration needed. Third-party cameras plugged into an Ozma NVR's PoE ports are isolated by the NVR's own firewall; only the local Frigate instance reaches them.

**Hardware options:**

| Option | Hardware | Approx cost | Notes |
|--------|----------|-------------|-------|
| Ozma Camera | OPi5 + PoE out port, Hailo-8L or RK3588 NPU, designed enclosure | TBD | Single camera, Frigate on-device |
| Ozma NVR 4 | OPi5 + 4-port PoE switch, single enclosure | TBD | 4 cameras, 6 TOPS RK3588 NPU handles all inference |
| Ozma NVR 8 | OPi5 + 8-port PoE switch, single enclosure | TBD | 8 cameras, same compute |
| DIY (single cam) | RPi 5 + PoE HAT (~$85) | ~$85 | Add Hailo-8L M.2 HAT (~$70) for full NPU inference |
| DIY (NVR 4) | Orange Pi 5 + TP-Link TL-SG1005P + 3D-printed enclosure | ~$115 | OPi5 plugged into switch uplink; 6 TOPS NPU, PoE+ (802.3at) |
| Existing machine | Any Linux machine + Frigate in Docker | ~$0 | `pip install ozma-agent`, set `machine_class: camera` |

The OPi5 + TP-Link TL-SG1005P DIY NVR 4 is the reference design the product hardware is built from. 4× 1440p at 5–10fps detection is comfortably within the RK3588S VPU and 6 TOPS NPU budget.

### Camera Recording Storage

Recording trigger and storage destination are independent choices.

**When to record** (configured per camera):
- Motion-triggered — Frigate's default; efficient, covers most use cases
- Object detection — only record when a specific class is detected (person, car, vehicle, animal)
- Event-triggered — doorbell press, recognised face, alarm zone crossing
- Continuous — full-time recording to local storage

**Where to store** (multiple destinations can be active simultaneously):

| Destination | How it works |
|-------------|-------------|
| **Connect cloud** | NVR encrypts footage with the user's own key before upload. Connect stores only ciphertext — it cannot view, process, or hand over footage even under legal compulsion. Connect manages storage cycling. This is the easiest option and requires no local infrastructure. |
| **Local NVR storage** | Frigate records directly to an attached SSD or NVMe drive on the NVR. Works fully offline. |
| **NAS / network storage** | Frigate mounts an NFS or SMB share. For cameras on the local network this is direct. For remote cameras (holiday home, etc.) the footage travels over the Connect relay tunnel to the home NAS — the camera's remote location is transparent. |
| **S3-compatible storage** | Frigate's S3 recording backend supports Backblaze B2, Wasabi, Minio, or AWS S3. Optional client-side encryption before upload for non-Connect destinations. |
| **Connect cache/backup** | An encrypted backup copy stored in Connect alongside any primary storage. Provides redundancy against local storage loss — including the scenario where an intruder takes the NVR, in which case the footage of the intrusion is still in Connect. |

**Zero-knowledge encryption.** The encryption key for Connect cloud recording is generated on the user's controller and never leaves their devices. Connect receives ciphertext and a nonce. There is no server-side decrypt path — this is architecturally enforced, not a policy statement.

### Bridge Nodes

Bridge nodes (`machine_class: "bridge"`) are satellite WiFi access points for larger houses where a single controller AP doesn't provide full coverage. They register with the controller as managed nodes and are configured from the dashboard like any other hardware.

**What a bridge node provides:**
- **IoT SSID** — default-deny, per-device firewall rules, onboarding workflow (device joins onboarding SSID → profiled → moved to IoT SSID with appropriate outbound rules)
- **Client SSID extension** — optional second radio for normal client traffic, extending the main network's reach
- **Mesh backhaul** — 802.11s or WDS back to the controller, single uplink Ethernet, or PoE-powered wall mount

For smaller deployments the controller's built-in hostapd AP (via USB WiFi dongle) is sufficient. The Ozma Bridge is for houses where you need managed IoT coverage in a garage, garden, utility room, or second floor that the controller can't reach.

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
