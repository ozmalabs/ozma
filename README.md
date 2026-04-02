# Ozma

> **Easy things automatic. Hard things easy. Amazing things possible.**

A software-defined KVM fabric. A small node device (SBC, RISC-V board, or emulated VM) attaches to a target PC via USB and presents itself as a keyboard, mouse, and audio device. A central Controller manages routing — which node feeds which target — over a local network.

No signal interruption on switch. No host software on the target. Works with any OS that supports standard USB device classes.

```
┌─────────────────────────────────────┐
│  Controller  (Linux, port 7380)     │
│  REST API · WebSocket · mDNS        │
└────────┬────────────────┬───────────┘
         │                │
┌────────┴───────┐  ┌─────┴──────────┐
│  Node          │  │  Soft Node     │
│  (RISC-V / SBC)│  │  (QEMU VM)     │
│  USB HID gadget│  │  QMP + HID     │
└────────┬───────┘  └─────┬──────────┘
         │ USB-C           │ USB/IP
┌────────┴───────┐  ┌─────┴──────────┐
│  Target PC A   │  │  Target PC B   │
│  (sees: HID,   │  │  (any desktop) │
│   audio gadget)│  │                │
└────────────────┘  └────────────────┘
```

## Repository layout

```
controller/     Python FastAPI controller daemon
node/           Python node listener (runs on the node device)
softnode/       QEMU-based soft node (node emulated as a VM)
agent/          Host agent (runs inside the target machine's OS)
tinynode/       Embedded platform support (Milk-V Duo S, RPi, Teensy) [submodule]
protocol/       Wire protocol specifications [submodule]
site/           Cloudflare Pages static web UI [submodule]
docs/           Architecture, protocols, security, getting started
dev/            Development harness: QEMU VMs, build scripts, Makefile
demo/           Demo orchestration scripts
tests/          Automated tests (E2E, unit, integration)
firmware/       ESP32 screen firmware
renderer/       Node.js screen rendering service
```

## Quick start (dev harness)

The dev harness emulates the full hardware stack with QEMU VMs.

### Prerequisites

```bash
cd dev
make deps          # check: qemu-system-riscv64, qemu-system-x86_64, usbip, python3
make ssh-key       # generate SSH key for VM access
make build-node-image   # build Alpine RISC-V disk image (requires sudo, ~5 min)
```

### Start everything

```bash
# Terminal 1: controller
cd controller && uv pip install -r requirements.txt
python -m uvicorn main:app --port 7380

# Terminal 2: soft nodes (vm1, vm2)
cd demo && bash start_vms.sh

# Terminal 3: RISC-V node VM + USB chain
cd dev
make node-vm       # start RISC-V VM (waits ~15s to boot)
make connect-vms   # setup USB gadget → USB/IP → host vhci → vm1
```

The RISC-V node will register with the controller as `ozma-riscv-node` with `capabilities: ["hid"]`. vm1 and vm2 register as soft nodes with `capabilities: ["qmp"]`.

### Useful dev commands

```bash
make logs           # tail node.py inside the RISC-V VM
make shell-node     # SSH into the RISC-V VM
make disconnect-vms # detach USB gadget from vm1
make stop           # stop all VMs
```

## Components

### Controller (`controller/`)

FastAPI daemon that manages the node inventory, routes HID input to the active node, and exposes a REST + WebSocket API on port 7380.

```bash
cd controller
uv pip install -r requirements.txt
python -m uvicorn main:app --port 7380 --reload
```

Key endpoints:
- `GET /api/v1/nodes` — list registered nodes
- `POST /api/v1/nodes/register` — node self-registration
- `POST /api/v1/switch/{node_id}` — set active node
- `WS /api/v1/events` — real-time event stream

### Node (`node/`)

Python daemon that runs on the node device. Receives HID packets over UDP (port 7331) and writes them to the USB HID gadget (`/dev/hidg0`, `/dev/hidg1`). Registers with the controller via mDNS or direct HTTP.

```bash
python3 node/node.py \
    --name my-node \
    --register-url http://controller:7380
```

The node requires a Linux USB HID gadget stack: `dummy_hcd` + ConfigFS for dev/test, or the hardware UDC on a real SBC.

### Soft Node (`softnode/`)

Emulates a node using a QEMU VM. Instead of writing to `/dev/hidg0`, HID input is forwarded to the VM via QMP (`usb-host` device hotplug or QMP key injection). Used for testing without hardware.

```bash
python3 softnode/soft_node.py \
    --name vm1 \
    --port 7332 \
    --qmp /tmp/ozma-vm1.qmp \
    --vnc-host 127.0.0.1 \
    --vnc-port 5901
```

### TinyNode (`tinynode/`) — submodule

Platform-specific support for embedded targets: Milk-V Duo S (RISC-V), Raspberry Pi, and Teensy 4.1. See [`tinynode/README.md`](tinynode/README.md).

### Protocol (`protocol/`) — submodule

Wire protocol specifications for all Ozma communication channels (HID UDP, control REST/WS, audio VBAN/Opus, video MJPEG/H.265, OTA, presence). See [`protocol/README.md`](protocol/README.md).

## Dev harness: RISC-V USB gadget chain

The dev harness implements the real hardware path using QEMU + USB/IP:

```
RISC-V VM (dummy_hcd + ConfigFS HID gadget + node.py)
  → usbipd (inside VM, TCP 3240, SLIRP-forwarded to host)
  → host vhci_hcd (usbip attach -r 127.0.0.1)
  → QEMU usb-host (QMP device_add → vm1 USB EHCI)
  → vm1 sees USB HID keyboard + mouse (1d6b:0104)
```

The cross-compiled USB gadget kernel modules (`dummy_hcd`, `libcomposite`, `usb_f_hid`, `usbip_host`, etc.) are built by `dev/kernel-build/build-gadget-modules.sh` and baked into the RISC-V image.

See [`dev/README.md`](dev/README.md) for the full setup guide.

## Network ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 7380 | TCP | Controller REST API + WebSocket |
| 7331 | UDP | HID input (keyboard/mouse packets to node) |
| 7332–7339 | UDP | Soft node HID ports (one per VM) |
| 7382 | TCP | Node HTTP API (USB info, status) |
| 3240 | TCP | USB/IP (usbipd inside RISC-V VM → host) |
| 2222 | TCP | RISC-V VM SSH (SLIRP forward) |

See [`protocol/specs/00-ports.md`](protocol/specs/00-ports.md) for the full port registry.

## Documentation

- [Architecture](docs/architecture.md) — system overview, three-layer model, data paths
- [Protocols](docs/protocols.md) — wire protocol specs, packet formats, REST API
- [Security](docs/security.md) — device identity, WireGuard mesh, enrollment, OTA signing
- [Getting Started](docs/getting-started.md) — dev harness setup guide

## License

AGPL-3.0 with plugin exception. See `COPYING` for full text.

The ozma platform is free software under the GNU Affero General Public License v3.
Third-party plugins (loaded via the plugin API) may be any license.

Hardware designs (PCB, enclosures): proprietary.
Documentation: CC-BY-4.0.

Copyright (C) 2024-2026 Ozma Labs Pty Ltd.
