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
