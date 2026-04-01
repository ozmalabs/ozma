# Ozma Proxmox Plugin — Specification

## Vision

Every Proxmox VM is an ozma node. Every physical machine with a hardware node
is an ozma node. Same dashboard, same API, same RPA, same AI agent. Physical
or virtual — the management plane is identical.

The Proxmox plugin integrates ozma directly into the hypervisor, eliminating
the standalone soft node process. VMs get first-class display capture (D-Bus
Scanout), native input injection (D-Bus Keyboard/Mouse), multi-monitor support,
surround audio, GPU passthrough with virtual display, and live migration.

## Architecture

```
Proxmox Cluster
  │
  ├─ Node A (physical host)
  │   ├─ Ozma Plugin (Perl + Python)
  │   │   ├─ VM "workstation-1"
  │   │   │   ├─ Display: D-Bus Console_0 (27" center)
  │   │   │   ├─ Display: D-Bus Console_1 (27" left)
  │   │   │   ├─ Display: D-Bus Console_2 (27" right)
  │   │   │   ├─ Audio: 5.1 surround (6 channels)
  │   │   │   ├─ GPU: VFIO passthrough (RTX 4090)
  │   │   │   │   └─ IddCx virtual display driver (in VM)
  │   │   │   ├─ Input: D-Bus Keyboard + Mouse
  │   │   │   └─ Agent: ozma-agent.exe (inside VM)
  │   │   │
  │   │   └─ VM "server-2"
  │   │       ├─ Display: D-Bus Console_0 (single virtio-gpu)
  │   │       ├─ Audio: stereo
  │   │       └─ Input: D-Bus
  │   │
  │   └─ Hardware Node (USB to physical machine "colo-server")
  │       ├─ Display: HDMI capture card
  │       ├─ Audio: HDMI audio extract
  │       └─ Input: USB HID gadget
  │
  ├─ Node B (physical host)
  │   └─ ... (more VMs + hardware nodes)
  │
  └─ Ozma Controller (runs on any node or as a container)
      ├─ Dashboard: unified view of all machines
      ├─ Scenarios: switch between any machine (VM or physical)
      ├─ AI Agent: control any machine via MCP
      └─ Streaming: all display feeds in one place
```

## What the plugin manages

### Per-VM configuration

The plugin adds an ozma configuration section to each VM:

```yaml
# /etc/pve/qemu-server/100.conf (Proxmox VM config)
ozma:
  enabled: true
  name: "workstation-1"
  displays:
    - head: 0
      resolution: 2560x1440
      refresh: 144
      position: center    # for display topology
    - head: 1
      resolution: 2560x1440
      refresh: 60
      position: left
    - head: 2
      resolution: 2560x1440
      refresh: 60
      position: right
  audio:
    channels: 6           # 5.1 surround
    format: s16le
    rate: 48000
  gpu:
    passthrough: true
    vfio_device: "0000:01:00.0"
    virtual_display: true  # install IddCx driver
  agent:
    auto_install: true
    controller_url: "https://ozma.local"
  scenario_group: "desks"
```

### VM lifecycle hooks

| Event | Plugin action |
|-------|---------------|
| VM create | Configure virtio-gpu heads, D-Bus display, audio devices, USB |
| VM start | Start display listener (D-Bus Scanout), register with controller |
| VM stop | Deregister from controller, clean up streams |
| VM migrate | Transfer display listener to destination host |
| VM snapshot | Include ozma config in snapshot |
| VM backup | Include ozma state in PBS backup |
| VM clone | Generate new node ID, preserve display config |

### Display management

**Scenario A: Virtual GPU (virtio-gpu)**

```bash
# Plugin generates QEMU args:
-device virtio-gpu-pci,id=vga0,max_outputs=3
-display dbus
```

Each head is a D-Bus Console. The plugin's display listener receives
`Scanout` frames for each head independently.

**Scenario B: GPU passthrough (VFIO)**

```bash
# Plugin generates QEMU args:
-device vfio-pci,host=01:00.0,id=gpu0,multifunction=on,x-vga=on
-display none     # no QEMU display — GPU renders directly
```

The agent inside the VM handles display:
1. Plugin auto-installs IddCx virtual display driver
2. Agent captures via DXGI Desktop Duplication
3. Agent streams to controller via HTTP

**Scenario C: Dual GPU (virtual + passthrough)**

```bash
# QEMU display on virtio-gpu (for BIOS/boot)
-device virtio-gpu-pci,id=vga0,max_outputs=1
# Physical GPU for guest rendering
-device vfio-pci,host=01:00.0,id=gpu0
-display dbus     # captures virtio-gpu only
```

Virtio-gpu shows BIOS and boot sequence. Once the OS loads the passthrough
GPU driver, the agent switches to DXGI capture of the real GPU output.
The controller seamlessly transitions between the two feed sources.

### Audio management

**Multi-channel audio:**

```bash
# Plugin generates QEMU args for 5.1:
-audiodev pipewire,id=audio0,out.name=ozma-vm100-front
-device intel-hda -device hda-output,audiodev=audio0    # front L+R
-audiodev pipewire,id=audio1,out.name=ozma-vm100-rear
-device hda-output,audiodev=audio1                       # rear L+R
-audiodev pipewire,id=audio2,out.name=ozma-vm100-center
-device hda-output,audiodev=audio2                       # center + LFE
```

Each audio stream maps to a PipeWire sink. The audio router combines them
for the active scenario's output (speakers, AirPlay, headphones).

**Inside the VM:** Windows sees 3 audio outputs. Configure as 5.1 surround
via Windows Sound Settings or use a virtual audio device that presents
a single 5.1 endpoint.

### Input management

All input goes through D-Bus — no VNC, no evdev hack, no QMP:

```python
# Keyboard
gdbus call --session --dest org.qemu \
    --object-path /org/qemu/Display1/Console_0 \
    --method org.qemu.Display1.Keyboard.Press "uint32 30"  # 'a'

# Mouse (absolute positioning)
gdbus call --session --dest org.qemu \
    --object-path /org/qemu/Display1/Console_0 \
    --method org.qemu.Display1.Mouse.SetAbsPosition "uint32 500" "uint32 300"
```

For multi-monitor: input targets a specific console. Mouse moves between
consoles based on the display topology (edge crossing).

## Plugin structure

```
/usr/share/perl5/PVE/API2/Ozma.pm           — Proxmox REST API extension
/usr/share/perl5/PVE/QemuServer/Ozma.pm     — QEMU config hooks
/usr/share/perl5/PVE/Ozma/DisplayListener.pm — D-Bus display capture manager
/usr/share/javascript/proxmox-widget-toolkit/ozma/  — Web UI extension
/usr/lib/ozma-proxmox/
    display-listener.py      — D-Bus Scanout frame receiver (Python 3.12+)
    input-proxy.py           — D-Bus keyboard/mouse proxy
    audio-router.py          — PipeWire multi-channel routing
    agent-installer.py       — Auto-install ozma agent in VMs
/var/lib/ozma/
    streams/                 — Live MJPEG/HLS streams per VM
    config/                  — Per-VM ozma configuration
    frames/                  — Latest frame per display head
```

### Proxmox Web UI extension

The plugin adds an "Ozma" tab to each VM's management page:

- **Live view** — all display heads rendered in the browser
- **Interactive control** — click and type directly (via D-Bus)
- **Display layout** — drag monitors to arrange topology
- **Audio** — channel assignment, volume, routing
- **Agent** — status, install, update, UI hints
- **Scenarios** — quick-switch from the VM page
- **RPA** — run automation scripts, test suites
- **Metrics** — GPU temp, CPU, RAM, network from inside the VM

### Proxmox Cluster integration

- **HA (High Availability)** — ozma follows VM migration automatically
- **Ceph/ZFS storage** — VM disks on shared storage for migration
- **SDN** — ozma traffic on dedicated VLAN (display streams, audio, HID)
- **Backup** — PBS backs up VMs with ozma config; restore recreates the full setup
- **Permissions** — Proxmox user/group/pool permissions apply to ozma actions

## Relationship to ozma controller

The Proxmox plugin does NOT replace the ozma controller. It supplements it:

| Component | Role |
|---|---|
| **Proxmox plugin** | Manages VM lifecycle, display capture, input injection per-VM |
| **Ozma controller** | Scenarios, audio routing, control surfaces, AI agent, dashboard, RPA |

The plugin registers each VM as a node with the controller via the standard
mDNS/HTTP registration path. The controller sees Proxmox VMs identically
to hardware nodes.

## Installation

```bash
# On each Proxmox node:
apt install ozma-proxmox-plugin

# Or manually:
dpkg -i ozma-proxmox-plugin_1.0.0_amd64.deb
systemctl restart pvedaemon
systemctl restart pveproxy
```

The package:
- Installs Perl modules into PVE's module path
- Installs the Python display/input/audio services
- Adds the web UI extension
- Creates systemd services for the display listener
- Configures PipeWire for multi-channel audio

## Pricing

The Proxmox plugin is part of **Ozma Business** (proprietary).

- **Open source (AGPL):** Controller, nodes, soft nodes, agent — everything works
  without Proxmox. Self-host, unlimited nodes, no limits.
- **Connect Pro ($1/node/mo):** Relay, HTTPS, backup, AI credits
- **Connect Team ($2/node/mo):** Fleet policies, compliance, multi-desk
- **Connect Business ($4/node/mo):** Proxmox plugin, AD/Entra, provisioning bay,
  audit export, SLA, SSO
- **Proxmox plugin standalone ($3/node/mo):** Just the plugin, no Connect

The plugin is the wedge into the enterprise/MSP market. Proxmox shops get
ozma's full RMM + KVM capability integrated into their existing infrastructure.

## Implementation phases

### Phase 1: Core plugin (V1.5)
- Perl hooks for VM lifecycle (start/stop/migrate)
- Display listener (D-Bus Scanout) — single head
- Input proxy (D-Bus Keyboard/Mouse)
- Registration with ozma controller
- Basic Proxmox web UI tab

### Phase 2: Multi-monitor + audio (V1.6)
- Multiple virtio-gpu heads per VM
- Display topology configuration in Proxmox UI
- Multi-channel PipeWire audio routing
- Edge crossing between display heads

### Phase 3: GPU passthrough (V1.7)
- VFIO configuration automation
- IddCx virtual display driver auto-install
- Agent DXGI capture → controller stream
- Seamless transition between virtio-gpu and passthrough GPU

### Phase 4: Enterprise integration (V1.8)
- PBS backup with ozma config
- HA migration with display listener follow
- SDN VLAN configuration
- Proxmox cluster-wide ozma dashboard
- RBAC integration (Proxmox permissions → ozma permissions)
