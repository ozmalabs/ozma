# Protocols

All protocol specifications live in `protocol/specs/`. This document summarises each protocol, its packet format, and current status.

All protocol versions are currently `0.1` — **unstable, breaking changes expected**.

---

## Security Envelope

All protocols operate **inside a WireGuard mesh** (overlay network `10.200.0.0/16`, tunnel port UDP 51820). WireGuard provides mutual device authentication (Curve25519 keypairs) and encryption (ChaCha20-Poly1305) for every byte on every path. No individual protocol needs its own encryption layer.

See [security.md](security.md) for the full security architecture.

---

## Port Assignments

Port 51820 is the only port exposed on the physical network. All other ports are bound to the WireGuard overlay interface.

| Port | Proto | Direction | Purpose |
|---|---|---|---|
| **51820** | **UDP** | **Any ↔ Controller** | **WireGuard tunnel** |
| 7331 | UDP | Controller → Node | HID keyboard/mouse reports |
| 7332 | UDP | Controller → Node | MJPEG camera feed |
| 6980 | UDP | Bidirectional | VBAN uncompressed audio |
| 7340 | UDP | Bidirectional | Opus RTP compressed audio |
| 7380 | TCP | Any → Controller | REST API + WebSocket |
| 7381 | TCP | Any → Controller | MCP server (AI agent interface) |
| 7382 | TCP | Node → Controller | Node HTTP API |

---

## Discovery (mDNS/DNS-SD)

Nodes announce themselves using mDNS.

**Service type**: `_ozma._udp.local`

**Instance name format**: `ozma-<role>-<last4mac>`

**TXT record fields**:

| Key | Example | Meaning |
|---|---|---|
| `proto` | `ozma/0.1` | Protocol version |
| `role` | `node`, `video-node`, `room-mic` | Node function |
| `caps` | `hid,audio,camera` | Declared capabilities |
| `hid_port` | `7331` | HID listener port |
| `audio_port` | `6980` | Audio port |
| `hw` | `milkv-duo-s` | Hardware platform identifier |
| `fw` | `0.1.3` | Firmware version string |

**Controller behavior**:

1. Listens for `_ozma._udp.local` announcements on startup.
2. Validates `proto` field — rejects different MAJOR version.
3. Extracts capabilities and builds node inventory.
4. Re-queries every 60 seconds; marks nodes offline if they stop responding.
5. Emits `node.online` / `node.offline` WebSocket events on inventory changes.

---

## HID Protocol

```
Byte 0: packet type
  0x01 = keyboard
  0x02 = mouse

Keyboard payload (8 bytes):
  [modifier, 0x00, key1, key2, key3, key4, key5, key6]
  Standard HID boot protocol report.

Mouse payload (6 bytes):
  [buttons, x_lo, x_hi, y_lo, y_hi, scroll]
  X/Y are 0–32767 absolute coordinates.
```

The Controller captures evdev events, translates to HID reports, and sends them over UDP to the active node's HID port.

---

## VBAN Audio

VBAN is a simple audio-over-UDP protocol. 28-byte header + raw PCM payload.

```
Offset  Len  Field
0       4    Magic "VBAN"
4       1    Sample rate index
5       1    Samples per frame minus 1
6       1    Channels minus 1 (0 = mono, 1 = stereo)
7       1    Format (low nibble) + codec (high nibble). 0x01 = PCM int16
8       16   Stream name (null-padded ASCII)
24      4    Frame counter (uint32 little-endian)
28+     N    Raw PCM samples (interleaved channels, little-endian int16)
```

Typical config: 48000 Hz, 256 samples/frame, stereo, PCM int16.
Frame rate = 48000/256 ≈ 187.5 Hz, payload = 1024 bytes/frame.

---

## REST API

Base URL: `http://<controller>:7380/api/v1`

### Core endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/nodes` | List registered nodes |
| POST | `/nodes/register` | Node self-registration |
| POST | `/switch/{node_id}` | Set active node |
| GET | `/scenarios` | List scenarios |
| POST | `/scenarios` | Create scenario |
| POST | `/scenarios/{id}/activate` | Activate scenario |
| WS | `/events` | Real-time event stream |

### WebSocket Events

Events are JSON objects with a `type` field:

```json
{"type": "node.online", "node_id": "ozma-node-a3f2", "caps": ["hid", "audio"]}
{"type": "scenario.activated", "scenario_id": "workstation"}
{"type": "audio.volume_changed", "node_id": "vm1", "volume": 0.75}
```
