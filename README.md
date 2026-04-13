# Ozma

[![CI](https://github.com/ozmalabs/ozma/actions/workflows/ci.yml/badge.svg)](https://github.com/ozmalabs/ozma/actions/workflows/ci.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)
[![Python 3.11+](https://img.shields.io/badge/python-3.11%2B-blue.svg)](https://www.python.org/downloads/)
[![Platform: Linux](https://img.shields.io/badge/platform-Linux-lightgrey.svg)](https://kernel.org/)
[![PRs Welcome](https://img.shields.io/badge/PRs-welcome-brightgreen.svg)](https://github.com/ozmalabs/ozma/pulls)

> **Easy things automatic. Hard things easy. Amazing things possible.**

Ozma is a software-defined **KVMA router** — Keyboard, Video, Mouse, and Audio, routed over a network. A small node device (SBC, RISC-V board, or soft node) attaches permanently to a target machine via USB and presents itself as a keyboard, mouse, audio device, and network card. A Controller manages routing — switching focus changes only which node receives input. **The USB cable never moves.**

```
Physical keyboard/mouse → Controller (Linux, port 7380)
                                │
               ┌────────────────┴────────────────┐
               │  WireGuard mesh (10.200.0.0/16) │
               └────────────────┬────────────────┘
                                │
          ┌─────────────────────┼─────────────────────┐
          │                     │                     │
   ┌──────┴──────┐       ┌──────┴──────┐       ┌─────┴───────┐
   │  Node A     │       │  Soft Node  │       │  Agent      │
   │  (RISC-V    │       │  (QEMU VM)  │       │  (desktop   │
   │   SBC)      │       │  QMP + HID  │       │   OS)       │
   └──────┬──────┘       └──────┬──────┘       └─────┬───────┘
          │ USB-C (permanent)   │ USB/IP              │ virtual
   ┌──────┴──────┐       ┌──────┴──────┐       ┌─────┴───────┐
   │  Machine A  │       │  Machine B  │       │  Machine C  │
   │  (any OS)   │       │  (any OS)   │       │  (any OS)   │
   └─────────────┘       └─────────────┘       └─────────────┘
```

No signal interruption on switch. No host software required on the target for hardware nodes. Works with any OS that recognises standard USB HID, audio, and network gadgets.

---

## What Ozma provides

**Core KVMA**
- Switch keyboard, mouse, and audio between machines in under 5 ms
- No USB movement on switch — routing is purely in software
- Hardware nodes (RISC-V / SBC), soft nodes (QEMU VMs), and desktop agents (any OS)
- HDMI capture for streaming and remote viewing; agent-based virtual display (no HDCP issue)

**Audio**
- PipeWire graph routing with real-time watcher
- VBAN V0.3 for low-latency LAN audio between nodes
- AirPlay, Spotify Connect, RTP, ROC, and Snapcast output targets
- WirePlumber integration for hardware-native routing
- Room correction (sweep → FFT → parametric EQ → PipeWire filter-chain)

**Control surfaces**
- MIDI controllers (X-Touch, any CC-capable surface)
- Elgato Stream Deck (Mini, Original, XL, Pedal, Neo)
- OSC network control (TouchOSC, Lemur, any OSC client)
- Generic evdev (ShuttlePRO, foot pedals, macro pads)
- Gamepad support (Xbox, PlayStation, generic)

**Screen system**
- Three-tier rendering: server-rendered frames (Node.js), native on-device (ESP32), constrained displays
- 14 concrete drivers: Stream Deck, Corsair LCD, OLED, e-ink, LED matrix, and more
- Pluggable widget packs, downloadable from Ozma Connect

**AI agent control**
- MCP tool server (stdio + SSE, port 7381) for Claude Desktop / Code / remote agents
- Set-of-Marks vision, OmniParser, YOLO, and Ollama vision providers
- Autonomous RPA engine with visual regression test runner
- Screen reader (OCR + UI element detection)

**Networking and security**
- WireGuard mesh overlay (10.200.0.0/16) — controller, nodes, mobile clients
- Device enrollment with Ed25519 identity keys and admin approval
- Mobile app: WireGuard split tunnel + mTLS client certificates, one QR enrollment
- DNS integrity verification: resolver integrity, DNSSEC, NXDOMAIN hijacking, rebinding guard, captive portal detection
- Optional VPN: Tier 1 (home exit — genuinely no logs), Tier 2 (Connect relay, explicitly disclosed), Tier 3 (future geo-exit)
- 4-level device assurance model (software-only → TPM-attested) integrated into routing constraints

**Enterprise IT (optional modules)**
- Identity provider: Authentik-backed IdP, OIDC/SAML for LAN services, FreeIPA integration
- Compliance: Essential Eight, CIS, ISO 27001, SOC 2 evidence generated automatically
- MDM bridge: WireGuard profile push, device posture, offboarding
- SIEM, threat intelligence, and Virtual SOC
- Password manager: managed Vaultwarden (Bitwarden-compatible) on controller hardware
- ITSM: AI-backed L1/L2 agent, escalation policies, on-call management
- SaaS management: discovery, licence tracking, shadow IT, vendor risk

**Cameras**
- `machine_class="camera"` node type: plug-and-play Frigate integration via mDNS
- Zero-knowledge encrypted footage backup to Ozma Connect
- Consumer gifting use case: technical installer, non-technical end user

---

## Repository layout

```
controller/       Python — Controller daemon (port 7380)
  main.py           entry point; wires all managers together
  api.py            FastAPI REST + WebSocket
  state.py          AppState + NodeInfo dataclasses
  routing/          Intent-based routing graph (devices, links, constraints)
  scenarios.py      ScenarioManager; scenarios.json persistence
  hid.py            evdev capture → UDP forwarder
  audio.py          AudioRouter (PipeWire + VBAN)
  controls.py       Control surface abstraction
  agent_engine.py   AI agent control (MCP, SoM, vision, RPA)
  dns_verify.py     DNS integrity verification + rebinding guard
  auth.py           JWT + Ed25519, Argon2id, WireGuard bypass
  connect.py        Ozma Connect client (relay, backup, AI proxy)
  … (60+ modules — see CLAUDE.md for full index)

node/             Python — Hardware node daemon (runs on SBC)
softnode/         Python — Soft node (runs on hypervisor, targets QEMU VMs)
agent/            Python — Host agent (installs inside the target OS)
tinynode/         Embedded platform support (RISC-V, RPi, Teensy) [submodule]
protocol/         Wire protocol specifications [submodule]
firmware/         ESP32 screen firmware
renderer/         Node.js screen rendering service (port 7390)
docs/             Architecture, protocols, security, VPN, getting started
dev/              Development harness (RISC-V QEMU, Makefile, build scripts)
demo/             Demo orchestration scripts
tests/            Automated tests (E2E, unit)
```

---

## Quick start (dev harness)

The dev harness emulates the full hardware stack with QEMU VMs — no physical hardware needed.

### Prerequisites

```bash
# Required: qemu-system-riscv64, qemu-system-x86_64, usbip, python3, uv
cd dev
make deps        # check all prerequisites
make ssh-key     # generate SSH key for VM access
make build-node-image   # build Alpine RISC-V disk image (~5 min, requires sudo)
```

### Start the stack

```bash
# Terminal 1 — Controller
cd controller
uv pip install -r requirements.txt
python main.py

# Terminal 2 — Soft nodes (vm1, vm2)
bash demo/start_vms.sh

# Terminal 3 — RISC-V hardware node VM
cd dev && make node-vm
```

The controller starts on `http://localhost:7380`. The RISC-V node registers as `ozma-riscv-node`. VMs register as soft nodes via mDNS.

### Test switching

```bash
python tests/test_e2e_switching.py
```

### Dev shortcuts

```bash
make logs           # tail node.py logs inside the RISC-V VM
make shell-node     # SSH into the RISC-V VM
make stop           # stop all VMs
bash demo/start_vms.sh stop   # stop soft nodes
```

---

## Network ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 7380 | TCP | Controller REST API + WebSocket |
| 7331 | UDP | HID packets → hardware node |
| 7332–7339 | UDP | Soft node HID ports (one per VM) |
| 7381 | TCP | MCP server (SSE transport for AI agents) |
| 7382 | TCP | Node HTTP API (status, HLS stream) |
| 7390 | TCP | Screen rendering server (WebSocket, Node.js) |
| 7391 | TCP | Native screen rendering server (on-device) |
| 6980 | UDP | VBAN audio (per-node) |
| 3240 | TCP | USB/IP (RISC-V node → host, via SLIRP) |
| 2222 | TCP | SSH → RISC-V node VM |
| 51820 | UDP | WireGuard mesh |

---

## Documentation

| Document | Contents |
|----------|----------|
| [Architecture](docs/architecture.md) | System overview, three-layer model, mesh topology, scale limits |
| [Protocols](docs/protocols.md) | Wire protocol specs, HID packet format, REST API reference |
| [Security](docs/security.md) | Device identity, WireGuard mesh, enrollment, OTA signing, device assurance, mobile auth, DNS integrity |
| [VPN](docs/vpn.md) | Tiered VPN — what each mode provides and does not provide |
| [Getting Started](docs/getting-started.md) | Dev harness setup guide |

---

## License

**Code:** AGPL-3.0 with plugin exception. The controller, nodes, and agents are free software under the GNU Affero General Public License v3. Third-party plugins loaded via the plugin API may be any licence.

**Hardware designs** (PCB, enclosures): proprietary.

**Documentation:** CC-BY-4.0.

Copyright (C) 2024–2026 Ozma Labs Pty Ltd.
