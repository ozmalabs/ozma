# Hardware Reference

This guide covers recommended hardware for Ozma controllers and nodes. The dev
harness requires no hardware — use `demo/start_vms.sh` for a fully virtual stack.

> **These recommendations are subject to change.** Ozma's feature set is growing
> rapidly and the minimum viable hardware for each use case will be refined as we
> complete throughput and camera-count benchmarking on real hardware. Check this
> document before purchasing if you haven't looked recently.

---

## Open hardware principle

Ozma is open-source (AGPL-3.0) and is designed around open hardware where
possible. This means:

- No proprietary protocol licences — the DisplayPort receiver RTL in
  `hardware/rtl/dp_rx/` is written from the public VESA specification
- Open FPGA toolchains (yosys/nextpnr) preferred over vendor toolchains
- KiCad for PCB design
- Commodity hardware (TP-Link, Reolink, AliExpress SBCs) over locked ecosystems

This is a deliberate product position, not a cost optimisation.

---

## Controller hardware

The controller is the Linux machine that runs `controller/main.py`. What hardware
you need depends entirely on which features you use. The table below maps feature
sets to hardware tiers. **You do not need powerful hardware to start** — a basic
KVM switch with a few nodes runs fine on almost any x86_64 or ARM64 box.

### Feature tiers

| Tier | Features | Hardware target |
|------|----------|----------------|
| **Minimal** | KVM switching, soft/virtual nodes, web UI, API | Any dual-core, 4 GB RAM, 1 NIC |
| **Standard** | + HDMI capture, audio routing, screen rendering, HA, basic Frigate | N100, 8 GB RAM, 2× NIC |
| **Full stack** | + IoT router, Frigate multi-camera, media server, AI agent | N150, 16 GB RAM, 3+ NIC ports |
| **Heavy** | + many cameras, many KVM nodes, local vision inference, broadcast | Core Ultra / Ryzen 7000+, 32 GB, dGPU |

---

### Tier 1 — Minimal (KVM only)

Any machine capable of running Python 3.11 on Linux. No special hardware
required. Suitable for: trying Ozma, soft-node/VM switching, no HDMI capture.

Examples: Raspberry Pi 4 (4 GB), any Celeron/Pentium NUC, a VM on an existing
server.

- RAM: 4 GB minimum
- Storage: 16 GB minimum (no recordings)
- NIC: 1× (gigabit is fine)
- No GPU, NPU, or multiple NICs required

---

### Tier 2 — Standard (~$150–250)

The right choice for most home and small-office setups: KVM switching with HDMI
capture, audio routing, Home Assistant, and Frigate with 2–4 cameras.

**Recommended: Intel N100 mini-PC** (Beelink EQ12, Trigkey G4, GMKtec G3, or
similar)

| Spec | Minimum | Recommended |
|------|---------|-------------|
| CPU | N100 (4-core Alder Lake-N) | N100 or N150 |
| RAM | 8 GB | 16 GB |
| NVMe | 256 GB | 512 GB |
| NIC | 2× 2.5GbE (built-in on most N100 units) | 2× 2.5GbE |
| GPU | Intel UHD (integrated, used for QuickSync H.265 encode) | same |

The N100's Intel QuickSync accelerates H.265 encoding for both KVM capture
streams and Frigate camera recordings. The integrated GPU handles this well up
to approximately 4–6 simultaneous 1080p streams — exact limits pending
benchmarking (see below).

For IoT VLAN support in appliance mode (using an existing managed switch), two
NICs is sufficient: one uplink to the LAN, one dedicated to the IoT VLAN trunk.
For router mode (controller is the gateway), a USB 3.0 → 2.5GbE adapter (~$15)
provides the third interface.

---

### Tier 3 — Full stack (~$200–350, recommended for most deployments)

The target hardware for running Ozma as a complete home infrastructure appliance:
KVM switch + IoT router + Frigate NVR (8–12 cameras) + Home Assistant + media
server, all on one box.

**Recommended: Intel N150 mini-PC, 16 GB RAM, 3+ NIC ports**

| Spec | Value |
|------|-------|
| CPU | Intel N150 (4-core, 3.6 GHz boost, Alder Lake-N) |
| RAM | 16 GB DDR5 |
| NVMe | 1 TB (for camera recordings — ~7–15 GB/day per camera at 1080p) |
| NIC | 2× 2.5GbE built-in + 1× USB 3.0 2.5GbE adapter, **or** 3-NIC model |
| GPU | Intel UHD Graphics (QuickSync + OpenVINO for Frigate detection) |

The N150 over the N100: slightly higher clock, same power envelope, same
QuickSync capability. The key upgrade here is RAM — 16 GB gives Frigate room
for detection models alongside the controller and HA without memory pressure.

**Three NIC ports** are the minimum for router mode with full VLAN separation:

| Port | Role |
|------|------|
| NIC 1 | WAN (uplink to modem/ISP) |
| NIC 2 | Main LAN (trunk to home switch) |
| NIC 3 | IoT VLAN (dedicated interface or trunk to IoT switch) |

If using a managed switch that handles VLAN trunking, two NICs suffice (WAN +
trunk). The third NIC becomes useful when you want hard physical separation
between the main LAN and IoT segments, or when adding a dedicated 2.4 GHz IoT
AP interface via USB.

---

### Tier 4 — Heavy (large deployments, many cameras, AI features)

For deployments with 12+ cameras, 10+ KVM nodes, local vision inference (no
cloud), or broadcast/OBS integration. The bottleneck shifts from CPU to GPU
and memory bandwidth.

**Example hardware:**

| | Option A | Option B |
|---|---|---|
| CPU | Intel Core Ultra 5 125H | AMD Ryzen 7 7840HS |
| RAM | 32 GB | 32 GB |
| NVMe | 2 TB | 2 TB |
| GPU | Intel Arc (integrated) or discrete | AMD Radeon 780M or discrete |
| NIC | 2.5GbE × 2 + 10GbE (add-in) | same |
| NPU | Intel AI Boost (11.5 TOPS) | AMD XDNA (16 TOPS) |

The NPU enables local OmniParser and Frigate detection without saturating the
iGPU. This is the recommended tier for the AI agent features and for deployments
where cloud vision assistance is not acceptable for privacy reasons.

---

### Performance limits (benchmarking in progress)

These limits are approximate and based on initial testing. Formal benchmarks
across all tiers are planned — this table will be updated as results come in.

| Metric | N100 (8 GB) | N150 (16 GB) | Core Ultra 5 |
|--------|-------------|--------------|-------------|
| KVM nodes (simultaneous HID) | TBD | TBD | TBD |
| HLS streams (simultaneous viewers) | TBD | TBD | TBD |
| Frigate cameras @ 1080p30, software detection | ~2–3 | ~4–6 | TBD |
| Frigate cameras @ 1080p30, OpenVINO (iGPU) | ~6–8 | ~10–14 | TBD |
| Frigate cameras @ 1080p30, NPU | N/A | N/A | TBD |
| Network throughput (router mode) | TBD | TBD | TBD |
| KVM capture + encode (1080p60 nodes) | TBD | TBD | TBD |

**Help wanted**: if you run Ozma on any of these hardware tiers, please report
your results in the GitHub issues with the `benchmarking` label so we can fill
in this table with real data.

---

### The complete appliance

One N150 box, running everything:

```
┌─────────────────────────────────────────────────────┐
│  Ozma Controller (N150, 16 GB, 1 TB NVMe)          │
│                                                     │
│  KVM switch        — control N machines via nodes  │
│  IoT router        — VLAN isolation, default-deny  │
│  Frigate NVR       — 8–12 cameras, local detection │
│  Home Assistant    — automation, device control    │
│  Jellyfin          — local media server            │
│  Immich            — local photo library           │
│  Audiobookshelf    — local audiobook server        │
│  Ozma Connect      — remote access, no cloud data  │
└─────────────────────────────────────────────────────┘
```

This replaces: your router, your NVR, your KVM switch, your media server, your
home automation hub, and the cloud subscriptions each of those would otherwise
require — with one box under your control, running open-source software, with
no vendor lock-in.

See [Network Architecture](network.md) for the router/IoT details and
[Camera Recommendations](cameras.md) for the Frigate camera setup.

---

---

## Node hardware overview

An Ozma node needs:

| Role | What | Port mode |
|------|------|-----------|
| HID gadget to target | USB-C or USB-A OTG | **device** (gadget) |
| HDMI capture card | USB-A host | **host** |
| Controller link | Ethernet or WiFi | network |

The OTG port must support `configfs` USB gadget mode (HID + UAC2). Most Allwinner
H6/H616/H618 and Rockchip RK3588 boards support this on mainline Linux.

---

## Option 1 — Cheapest DIY (Orange Pi Zero 3 + MS2109)

**Target cost: ~$28–35 landed**

### Bill of materials

| Part | Chip | Where | Approx cost |
|------|------|-------|-------------|
| Orange Pi Zero 3 (1 GB) | H618 | AliExpress / official store | $18–22 |
| USB HDMI capture dongle | MS2109 | AliExpress ("USB HDMI capture card") | $8–12 |
| USB-C cable (OTG, to target) | — | any | $1–2 |
| MicroSD ≥ 8 GB | — | any | $3–5 |
| Heatsink (recommended) | — | AliExpress | $1 |

### Why this board

The Zero 3 has a native port split that matches exactly what a node needs:

- **USB 3.0 OTG (USB-C)** → target machine (HID gadget + UAC2 audio gadget)
- **USB 2.0 host (USB-A)** → MS2109 capture card
- **Gigabit Ethernet** → controller link (no WiFi latency)
- H618 handles 1080p V4L2 → HLS encoding without thermal issues at node-typical load

No hub, no expansion board needed.

### Capture card notes

The MS2109 is a USB 2.0 UVC device. It captures:

- **Video**: up to 1080p30 (or 720p60) uncompressed over USB 2.0
- **Audio**: HDMI embedded audio via UAC — exposed as a separate ALSA device
- Appears as `/dev/video0` with no driver installation

The node's `capture.py` detects it automatically via V4L2 enumeration.

> If you need **1080p60**, use an **MS2130-based card** (~$15–20, USB 3.0).
> The Zero 3's USB-A host port is USB 2.0, so you'd need a USB 3.0 hub between
> the board and the card for full throughput. At that point, consider Option 2.

### Setup

```bash
# Flash Armbian (recommended) or official Orange Pi OS to SD card
# Enable USB gadget overlay in /boot/armbianEnv.txt:
#   overlays=usbhost2 usbhost3
# Then follow tinynode/README.md for gadget configfs setup
```

### Limitations

- 1080p30 max without a USB 3.0 hub
- No PCIe — NVME or GPU passthrough not possible
- UAC2 gadget audio output only (no line-in from node side)

---

## Option 2 — Magic Dock (Orange Pi 5)

**Target cost: ~$80–110 landed**

The Orange Pi 5 (RK3588S) doubles as a node **and** a USB-C dock for laptops. A
laptop plugs into the OPi5 via a single USB-C cable and gets: HID injection from
Ozma, display capture back to the controller, and (optionally) USB hub access to
peripherals wired to the node.

```
Laptop ──USB-C──▶ OPi5 (OTG/gadget: HID + UAC2 + charging)
                     │
                     ├─USB-A──▶ MS2130 capture ◀──HDMI── laptop (external output)
                     ├─USB-A──▶ USB hub (keyboard/mouse pass-through, etc.)
                     └─Ethernet──▶ controller
```

This replaces a $150+ Thunderbolt dock for machines where Thunderbolt isn't
available, while simultaneously being a full Ozma node.

### Bill of materials

| Part | Chip | Where | Approx cost |
|------|------|-------|-------------|
| Orange Pi 5 (4 GB) | RK3588S | AliExpress / official store | $65–80 |
| USB HDMI capture dongle | MS2130 | AliExpress | $15–20 |
| USB-C cable (OTG + power, to laptop) | — | any (E-mark rated) | $3–5 |
| NVMe SSD (optional, for OS) | — | any M.2 2280 | $15–25 |
| Heatsink + fan | — | included or AliExpress | $5–8 |

### Why this board

- **RK3588S** — 8-core (4× A76 + 4× A55) + 6 TOPS NPU; handles 1080p60 capture,
  re-encoding, and local OmniParser vision inference simultaneously
- **USB 3.1 OTG (USB-C)** — gadget mode for HID + UAC2 + optional power delivery
  negotiation (laptop charges through the node)
- **USB 3.1 host ports** — MS2130 at full 1080p60 bandwidth
- **PCIe 2.0 ×1** — future: NVMe boot drive, or PCIe capture card
- **NPU** — future: on-device vision inference for `agent_engine.py` without cloud

### Magic dock use case

The OPi5 presents itself to the laptop as:
1. A USB HID composite device (keyboard + mouse, via configfs gadget)
2. A UAC2 audio device (speakers + mic, via configfs gadget)
3. (Optional) A USB Power Delivery source (laptop charging via negotiated 5V–20V)

The laptop's HDMI or USB-C DP-alt output connects to the MS2130. The OPi5 captures
it and streams to the controller exactly like any other node.

From the user's perspective: one cable to the laptop = full KVM integration with no
extra hardware on the desk.

### Limitations

- OTG and host are muxed on RK3588S — verify the specific OPi5 board revision
  supports simultaneous OTG + host (most do via an onboard USB mux)
- USB PD negotiation requires a PD-capable port and E-mark cable; without it the
  laptop gets 5V/0.9A (enough to trickle charge, not power a heavy laptop)
- Larger and hotter than the Zero 3 — fan recommended for sustained capture workloads

---

## Option 3 — Ozma Dock (custom PCB, V2.0)

**Target cost: ~$150–200 BOM in low volume**

A purpose-built dock that replaces a Thunderbolt dock while being a full Ozma node.
One cable from the laptop. No separate capture dongles. No HDMI cables. Arbitrary
number of display streams.

```
Laptop ──USB4──▶ [TPS65994AD]       USB4 PD + DP Alt Mode controller
                      │ DP lanes
                      ▼
                 [FPGA]              Open RTL DisplayPort receiver
                  │   │              AUX channel, 8b/10b, MSA parser,
                  │   │              MST branch device, frame buffer
                  │   └──▶ [external monitor outputs]
                  │
                  ▼ raw frames (PCIe or MIPI CSI)
                 [RK3588S]           Node daemon: HLS encode, HID, mDNS
                      │
                 Ethernet ──▶ controller
```

### Key chips

| Chip | Role | Notes |
|------|------|-------|
| TI TPS65994AD | USB4 + PD + DP Alt Mode | Handles USB4 negotiation, DP lane mux, PD up to 140W. Protocol stack in ROM — no USB4 firmware to write. |
| Lattice CrossLink-NX | DisplayPort receiver (open RTL) | 6.25 Gbps SerDes → DP 1.2 HBR2; covers 1080p60 and 4K30. See `hardware/rtl/dp_rx/`. |
| Rockchip RK3588S | Node SoC | Same chip as OPi5. H.265 hardware encode, 6 TOPS NPU, proven node daemon. |

**V1 target** (ECP5, open toolchain): DP 1.1 HBR1, 1080p30 single display.
**V2 target** (CrossLink-NX): DP 1.2 HBR2, 1080p60 / 4K30, MST multi-display.

### Open RTL DisplayPort receiver

The DisplayPort protocol above the SerDes is fully documented by VESA (public spec,
free registration). The entire receiver stack is implemented as open RTL:

```
hardware/rtl/dp_rx/
  aux_channel.v      1 Mbps Manchester-encoded AUX controller (I2C-like)
  dpcd_regs.v        DPCD register file (sink capabilities, link config)
  link_training.v    TPS1/TPS2/TPS3 pattern response, EQ via AUX replies
  decoder_8b10b.v    4-lane 8b/10b decode (standard, many open references)
  msa_parser.v       Main Stream Attribute extraction (resolution, depth, fps)
  video_framer.v     Pixel reassembly from transport packets → pixel clock + RGB
  mst_branch.v       Multi-stream topology management (V2 target)
  frame_buffer.v     Dual-port BRAM frame store, async read for SoC DMA
```

The hard SerDes in the FPGA handles clock/data recovery and equalization at the
wire level — that's unavoidable in any FPGA design. Everything above it is open.

### Multiple displays

USB4 carries DisplayPort MST natively. The FPGA acts as an MST branch device,
exposing N virtual displays to the laptop. Each stream is an independent capture
channel fed to the SoC. The controller sees N HLS streams from one node — one per
display. No additional hardware per monitor.

### Development path

```
Now      OPi5 + MS2130 (COTS)     Proves dock concept, validates node daemon
V1 HW    ECP5 + TPS65994 + RK3588 Open toolchain, DP 1.1, 1080p30, 100W PD
V2 HW    CrossLink-NX variant      DP 1.2, 1080p60 / 4K30, MST, 140W EPR PD
```

The OPi5 COTS work (Option 2) directly de-risks the dock: the node daemon, HID
gadget, and dock-mode UX are all proven before a single PCB is spun.

### PCB

- 6-layer controlled impedance (USB4 differential pairs need tight stackup)
- KiCad — fully open, files in `hardware/dock/`
- USB4 connector: USB-C receptacle with appropriate EMI shielding
- USB4 compliance: TPS65994 handles signaling; full TBT4 cert is optional / deferred

---

## Comparison

| | Zero 3 + MS2109 | OPi5 + MS2130 | Ozma Dock (V2.0) |
|---|---|---|---|
| Cost | ~$30 | ~$100 | ~$150–200 BOM |
| Max capture | 1080p30 | 1080p60 | 1080p60 / 4K30+ |
| Multi-display | Via separate capture | Via separate capture | Yes, MST (1 cable) |
| Laptop charging | No | Optional (PD) | Yes, up to 140W |
| NPU | No | 6 TOPS | 6 TOPS |
| Open RTL | N/A | N/A | Yes (DP receiver) |
| HDMI cables to target | Yes | Yes | No |
| Best for | Desktop targets, budget | Laptops, AI features | Laptops, product SKU |

---

## Not yet tested

The following combinations are candidates for future testing:

| SBC | Notes |
|-----|-------|
| NanoPi NEO3 | USB 3.0, small form factor, no WiFi |
| Radxa Zero 3W | Wireless-only, very small, good for embedded installs |
| Rock 5B | RK3588, full PCIe, more expensive than OPi5 |
| Raspberry Pi 5 | Good Linux support, limited USB gadget maturity |

---

## Tested capture cards

| Card | Chip | Max capture | USB | Status |
|------|------|-------------|-----|--------|
| Generic "4K HDMI" dongle | MS2109 | 1080p30 / 720p60 | 2.0 | Tested |
| Generic USB 3.0 capture | MS2130 | 1080p60 | 3.0 | Planned |

Audio capture (HDMI embedded audio via UAC) works on both. The node's `capture.py`
picks up the UAC audio device automatically alongside the V4L2 video device.
