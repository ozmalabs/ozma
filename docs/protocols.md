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
| 443 | TCP | Any → Controller | HTTPS (service proxy, when certs available) |

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

### User management endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/users` | read | List all users |
| GET | `/users/me` | read | Current authenticated user |
| GET | `/users/{id}` | read | Get user by ID |
| POST | `/users` | admin | Create user |
| PUT | `/users/{id}` | write | Update user (own profile) or admin (any) |
| DELETE | `/users/{id}` | admin | Delete user |

### Service proxy endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/services` | read | List registered services |
| GET | `/services/{id}` | read | Get service details |
| POST | `/services` | write | Register a service |
| PUT | `/services/{id}` | write | Update service |
| DELETE | `/services/{id}` | write | Remove service |
| GET | `/services/{id}/health` | read | On-demand health check |

### Sharing endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/shares` | read | List grants (given + received) |
| GET | `/shares/{id}` | read | Get grant (grantor/grantee/admin only) |
| POST | `/shares` | write | Create share grant |
| DELETE | `/shares/{id}` | write | Revoke grant (grantor/admin only) |
| GET | `/peers` | read | List linked peer controllers |
| POST | `/peers` | admin | Link a peer controller |
| DELETE | `/peers/{id}` | admin | Unlink peer |

### Routing graph endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/graph` | read | Full routing graph (devices + links) |
| GET | `/graph/devices` | read | List all graph devices |
| GET | `/graph/devices/{id}` | read | Get device by ID |
| GET | `/graph/links` | read | List all graph links |

### Routing engine endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/routing/intents` | read | List all built-in intents |
| GET | `/routing/intents/{name}` | read | Get intent by name |
| GET | `/routing/explain` | read | Explain current routing decision |
| GET | `/routing/feasibility` | read | Check path feasibility |
| GET | `/routing/pipelines` | read | List cached pipeline recommendations |
| GET | `/routing/simulate` | read | Simulate routing for a given intent |
| GET | `/routing/measurement_engine` | read | Measurement engine status |
| GET | `/routing/binding_loop` | read | Binding loop status |
| GET | `/routing/bindings` | read | List all registered bindings |
| GET | `/routing/bindings/current` | read | Currently active binding + intent |
| POST | `/routing/evaluate` | write | Evaluate intents against current state |
| POST | `/routing/probe/{link_id}` | write | Trigger on-demand ICMP probe for a link |
| POST | `/routing/bindings/evaluate` | write | Run one binding evaluation cycle |

### Monitoring endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/monitoring/journal` | read | Query monitoring journal (type/device/severity/since filters) |
| GET | `/monitoring/metrics/{device_id}` | read | All metric series for a device |
| GET | `/monitoring/health` | read | Aggregated system health summary |
| GET | `/monitoring/trends` | read | Active trend alerts |
| GET | `/monitoring/link/{link_id}/history` | read | Link metric history (`?metric=latency_ms&tier=1&limit=N&since=T`) |

### External publishing endpoints

| Method | Path | Scope | Description |
|--------|------|-------|-------------|
| GET | `/publish` | read | List published services |
| POST | `/publish` | write | Publish a service externally |
| PUT | `/publish/{id}` | write | Update (public mode requires admin + confirmation) |
| DELETE | `/publish/{id}` | write | Unpublish |

### Identity Provider endpoints

These are mounted at the root, not under `/api/v1`:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/.well-known/openid-configuration` | OIDC discovery document |
| GET | `/auth/jwks` | JSON Web Key Set |
| GET | `/auth/login` | Login page |
| POST | `/auth/login` | Password authentication |
| GET | `/auth/login/{provider}` | Social login redirect |
| GET | `/auth/callback/{provider}` | Social login callback |
| POST | `/auth/logout` | End session |
| POST | `/auth/token` | OIDC token endpoint |
| GET | `/auth/userinfo` | OIDC userinfo endpoint |

### WebSocket Events

Events are JSON objects with a `type` field:

```json
{"type": "node.online", "node_id": "ozma-node-a3f2", "caps": ["hid", "audio"]}
{"type": "scenario.activated", "scenario_id": "workstation"}
{"type": "audio.volume_changed", "node_id": "vm1", "volume": 0.75}
{"type": "remote_desktop.consent_request", "session_id": "...", "node_id": "..."}
{"type": "agent.approval_required", "action_id": "...", "action": "click", "node_id": "..."}
{"type": "user.created", "user": {"id": "...", "username": "alice", ...}}
{"type": "user.deleted", "user_id": "..."}
{"type": "service.registered", "service": {"id": "...", "name": "Jellyfin", ...}}
{"type": "service.removed", "service_id": "..."}
{"type": "share.created", "grant": {"id": "...", "grantor_user_id": "...", ...}}
{"type": "share.revoked", "grant_id": "..."}
{"type": "peer.linked", "peer": {"id": "...", "name": "...", ...}}
{"type": "peer.unlinked", "controller_id": "..."}
{"type": "service.published", "entry": {"id": "...", "mode": "private", ...}}
{"type": "service.unpublished", "entry_id": "..."}
```

---

## Encrypted Wire Format

All node-to-controller traffic uses AEAD encryption after session establishment.

```
Byte 0:      Version (0x01)
Byte 1:      Packet type (plaintext, in AAD)
Bytes 2-9:   Nonce counter (8 bytes, big-endian, monotonic)
Bytes 10-N:  Ciphertext + 16-byte Poly1305 MAC
```

- **AEAD cipher**: XChaCha20-Poly1305 (libsodium)
- **Key derivation**: HKDF-SHA256 from X25519 DH shared secret
- **Nonce**: 16-byte seed (from HKDF) + 8-byte counter = 24 bytes
- **AAD**: version + packet_type + counter
- **Overhead**: 26 bytes per packet (1 ver + 1 type + 8 counter + 16 MAC)
- **Replay protection**: Sliding window (64 for HID, 512 for audio)

### Packet Types

| Type | Value | Payload |
|------|-------|---------|
| Keyboard | 0x01 | 8-byte HID boot protocol report |
| Mouse | 0x02 | 6-byte absolute mouse report |
| Audio | 0x03 | VBAN or Opus frame |
| Control | 0x04 | JSON control message |
| Keepalive | 0xFF | Empty |

### Session Establishment

1. Controller sends: version + ephemeral X25519 pubkey + Ed25519 signature + certificate + timestamp
2. Node verifies certificate against mesh CA, verifies signature
3. Node responds: version + ephemeral pubkey + signature + certificate + timestamp
4. Both compute X25519 DH → HKDF-SHA256 → session keys (separate send/recv)
5. Channel binding: both verify a session ID derived from the transcript hash

### Message Versioning

The version byte (`0x01`) allows future protocol evolution. Nodes and controllers negotiate the highest mutually-supported version during session establishment. The current version is `0x01` — breaking changes will increment this.

### Future Evaluation

The current wire format uses `struct.pack()` for binary encoding. This was chosen for:
- Minimal overhead (26 bytes vs ~50+ for protobuf framing)
- Direct portability to Rust (`sodiumoxide` crate, byte-level compatibility)
- No external dependencies or code generation

For V1.2 (native game streaming protocol), we may evaluate:
- **Protobuf**: Better schema evolution, but adds codegen dependency and framing overhead
- **FlatBuffers**: Zero-copy reads, good for streaming, but more complex API
- **Current approach**: Sufficient for HID/audio/control; streaming may benefit from richer framing

Decision deferred to V1.2 implementation.
