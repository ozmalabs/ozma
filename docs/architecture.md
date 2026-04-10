# System Architecture

## Overview

Ozma is a **KVMA router** — a software-defined network fabric that routes Keyboard, Video, Mouse, and Audio signals between a Controller and any number of target machines. A **Controller** (any Linux machine on the local network) manages a set of **Nodes** (small SBCs, MCUs, or software agents) that each attach to a target PC via USB. The target PC sees the node as ordinary USB peripherals — keyboard, mouse, audio device, and camera — while all routing decisions live in the Controller.

Traditional KVM products are *switches*: hardware in the signal path that physically moves a connection. Ozma is a *router*: the connections are permanent and the signals travel over the network. This distinction matters:

- No signal interruption when switching — the display stays connected and the monitor never resyncs.
- No host software required on the target — the node looks like a standard USB device class.
- Audio is a first-class signal, not an afterthought — routed with the same precision as keyboard and video.
- The same platform runs across every use case: KVM, conference room, NVR, home server, digital signage, lecture capture, live production.

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

## Backup

### Default On, Opt-Out

When a node is added, the agent analyses its profile directory and immediately offers a backup plan with a concrete estimate:

> "We found ~8 GB of documents, photos, and settings (apps and caches excluded — reinstallable).
> With the default backup plan: 7 daily snapshots, 4 weekly, 12 monthly.
> Your files change ~40 MB/day. Estimated total storage: ~11 GB. Cost on Connect: ~$0.07/month."

One click to start. "Not now" resurfaces after 30 days. The estimate is computed from file modification timestamps before the first backup — no guessing.

### Backup Stack

- **Restic** — primary engine. Single Go binary, Linux/macOS/Windows/FreeBSD/OpenBSD, content-addressed deduplication, AES-256 encrypted repos, native S3/SFTP/local/Azure/GCS/REST support. Repository password derived from the controller master key — zero-knowledge on Connect.
- **ZFS/BTRFS snapshots** (via Snapper) — filesystem-consistent snapshots before backup when available.
- **partclone** — sector-aware disk imaging for full disk backup mode (only used blocks).

### Four Backup Modes

**"Backup my files"** (default) — profile directory tree, files modified in the last 90 days, preview before start.

**"Smart backup"** — same, plus aggressive exclusions of reinstallable content: `node_modules/`, `.cargo/`, `__pycache__/`, Steam game caches, `/Applications`, `C:\Program Files`, build directories, `.venv/`, Docker cache, VM images, files >500 MB, `.Trash`.

**"Backup the disk"** — `partclone` image stored in a Restic repo. Supports no-touch restore: the node presents a bootable USB (via `virtual_media.py` + USB mass storage gadget), the BIOS boots from it, and a recovery OS restores the image over the network with zero user interaction.

**"Advanced"** — arbitrary includes/excludes, Restic tags, custom schedule, custom retention policy. Full Restic surface.

### Adaptive Scheduling

Backup runs automatically when conditions are right — CPU low, home network, bandwidth available, not in a meeting, plugged in. It backs off when CPU spikes, a meeting starts, bandwidth is saturated, or the battery is low. The `--limit-upload` flag is set dynamically from available bandwidth. No configuration required; advanced users can override.

### Selective Restore

The restore path you want 99% of the time:

1. Select node → "Restore files"
2. Calendar timeline of snapshots
3. Browse directory tree at that point in time
4. Select files/folders → restore to original or new location

Full disk restore is a separate "Disaster Recovery" path. Ransomware recovery (restore to a pre-infection snapshot) is explicitly supported and called out in the UI.

### Retention

Default: `--keep-daily 7 --keep-weekly 4 --keep-monthly 12`. Runs automatically after each backup. Configurable per profile. Business plans use fixed, unalterable retention (see below).

### Application Inventory

An `apps_snapshot.json` is stored with every backup, capturing all installed applications via osquery and native package managers (apt/dnf/brew/winget/pkg, flatpak, snap, pip/cargo/npm globals). On restore: batch auto-install for package-manager apps + download links + manual list for the rest. Steam games are restored by App ID.

### Backup Destinations

Restic supports any destination — local disk, NAS (NFS/SMB), S3-compatible storage (B2, Wasabi, Minio, AWS), or Connect cloud. Multiple destinations can be active simultaneously. Connect is the default for new users: zero configuration, storage cycling managed automatically, footage encrypted with the user's own key before upload.

### Business Plans: Immutable Backups

Business backups use Restic append-only mode combined with S3 Object Lock (Compliance mode). Clients cannot delete history; not even Connect admins can alter snapshots within the retention window. A secondary admin domain owns the GFS rotation schedule; the user's account has write and read-only access.

| Actor | Can do | Cannot do |
|-------|--------|-----------|
| User | Add snapshots, restore | Delete snapshots, change retention |
| Connect support | Nothing | Alter retention, delete (Object Lock) |
| Connect backup service | GFS rotation on fixed schedule | Anything outside schedule |
| Attacker with machine access | Add new snapshots | Erase existing history |

This satisfies Essential Eight Maturity Level 3 for backup and recovery, and covers SOC 2, ISO 27001 A.12.3, PCI DSS 9.5/10, and HIPAA §164.308(a)(7). Legal holds can suspend GFS rotation for specific snapshots pending litigation.

### Platform Support

| Platform | Filesystem-aware | Disk restore | No-touch? |
|----------|-----------------|--------------|-----------|
| Linux | ZFS/BTRFS/LVM snapshot | partclone | ✅ Full |
| macOS (Intel) | APFS tmutil snapshot | asr from image | ✅ Full |
| macOS (Apple Silicon) | APFS tmutil snapshot | Limited (secure boot) | ⚠️ Partial |
| Windows | VSS `--use-fs-snapshot` | WinPE + `dism /Apply-Image` | ✅ Full |
| FreeBSD | ZFS first-class, UFS dump/restore | mfsBSD + zfs receive | ✅ Full |
| OpenBSD | FFS dump/restore | Install media + restore script | ✅ Full |

### Controller Self-Backup

The controller is a managed backup target like any other node. It backs up to any configured destination (ideally offsite). Controller config backup — mesh CA keypair, scenarios, scenarios — is part of the Connect encrypted backup and is recovered automatically when the controller master key is restored.

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

## Scale Limits and Future Topology

### Current Addressing Limits

The mesh overlay uses RFC 1918 private addressing with a fixed subnet-per-node-type scheme. Each node type occupies a `/24`, giving 254 usable addresses per type. The controller-to-controller peering overlay occupies a separate `/24`.

| Resource | Current allocation | Hard limit |
|---|---|---|
| Hardware Compute Nodes (per controller) | `10.200.1.0/24` | **254** |
| Room Mic devices (per controller) | `10.200.2.0/24` | **254** |
| Soft Nodes (per controller) | `10.200.3.0/24` | **254** |
| Virtual Nodes (per controller) | `10.200.4.0/24` | **254** |
| Camera Nodes (per controller) | `10.200.5.0/24` | **254** |
| Relay servers | `10.200.100.0/24` | **254** |
| Mobile app clients | `10.202.0.0/16` | **65,534** |
| Controllers per mesh | `10.201.0.0/24` | **254** |

Total addressable nodes per controller under the current scheme is approximately **1,270** across all types. The `10.200.0.0/16` overlay has 250+ unused `/24` slots, so new node types can be added without changing the addressing plan.

**WireGuard performance.** WireGuard's peer routing is O(n) — it searches the peer list for each packet's destination. In practice this is fast (a few microseconds per lookup), but at several hundred peers on a single interface, sustained HID traffic begins to show latency variability. The current architecture (nodes peer only with the controller, not with each other) keeps the peer count on each node at 1, and the controller at N peers. The practical WireGuard peer limit on a typical controller (N100-class CPU) is roughly **500–1,000 peers** before per-packet overhead becomes measurable.

**These limits are not a concern for the vast majority of deployments.** A typical setup has 2–10 compute nodes. Even large deployments — a university department, a broadcast studio, a server farm — rarely exceed 50–100 nodes per controller. The limits exist and should be known; they are not expected to bind in practice until Ozma reaches enterprise/fleet scale.

### Expansion Within the Current Scheme

Without any protocol changes, the existing `/16` overlay can accommodate more nodes per type by widening individual node-type subnets:

- Widening hardware nodes from `/24` to `/16` increases the limit from 254 to 65,534 for that type
- The full `10.200.0.0/16` has room for several `/16` node subnets before exhaustion

This is a configuration-level change to the address plan — no protocol changes required, no node firmware changes required.

### Future Path: Hierarchical Controller Topology

When a single controller's peer count approaches the practical WireGuard limit, the natural answer is hierarchical topology: a root controller manages a set of sub-controllers, each of which manages its own set of nodes. Routing between sub-meshes works over the existing WireGuard fabric — the mesh already provides IP-level routing between any two endpoints, regardless of which controller enrolled them.

```
Root Controller (10.200.0.1)
├── Sub-controller A (10.200.0.2) → nodes 10.200.1.0/24
├── Sub-controller B (10.200.0.3) → nodes 10.200.2.0/24
└── Sub-controller C (10.200.0.4) → nodes 10.200.3.0/24
```

Each sub-controller manages its own subnet allocation. The root controller only holds sub-controller peers (low N), while each sub-controller holds its own node peers (also low N). Scenario switching between nodes on different sub-controllers routes through the sub-controllers transparently — from a user perspective, all nodes are still in one flat inventory.

**Self-rearrangement.** A controller approaching its peer limit could automatically promote an eligible node (one with sufficient compute) to a sub-controller role, migrate a subset of its node registrations to it, and update the routing table. This is automated hierarchical splitting — the mesh topology changes without operator involvement, and users see no change in the node inventory. This is a planned capability for large fleet deployments.

### Future Path: Connect-Managed IPAM

For large deployments where multiple controllers form a mesh, Ozma Connect can act as an **IPAM coordinator**: each controller requests a subnet on mesh join, Connect allocates a non-overlapping block, and the allocation is stored in the Connect-backed config. This avoids the 254-controller limit of the current fixed `/24` scheme and eliminates the need for manual address planning in large organisations.

### Future Path: IPv6 ULA Overlay

The WireGuard mesh can be run over **IPv6 Unique Local Addresses** (fc00::/7, effectively /48 or /64 per controller). This provides:

- Effectively unlimited address space — no per-type subnet limits
- Easy hierarchical allocation: each controller gets a `/48`, each node type gets a `/64` within it
- No changes to the node or controller software beyond address family support

IPv6 ULA is the long-term addressing foundation for very large deployments and is the planned V2.0 mesh overlay. IPv4 support is retained for backward compatibility with hardware that does not support IPv6.

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
