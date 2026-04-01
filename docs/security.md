# Security Architecture

This document defines the Ozma security model: device identity, enrollment, transport encryption, control plane authentication, and OTA firmware signing.

The previous threat model — "trust the LAN because it is physically isolated" — is replaced here with a cryptographic trust model that works on a local isolated LAN, over a corporate VLAN, over a cloud relay, and across the internet. Physical isolation remains a valid and simpler deployment option, but the protocol no longer requires it.

---

## Guiding Principles

1. **Device identity is a keypair, not a location.** A node is trusted because it holds a private key whose corresponding public key the Controller has approved — not because it is at a particular IP address.
2. **Every data path is encrypted.** No Ozma traffic travels in plaintext outside the Controller host.
3. **Network topology is decoupled from trust.** The same node can be on the local LAN today and behind a 4G modem tomorrow; the security model does not change.
4. **Operator approves enrollment; the system handles the rest.** A human grants a node permission to join once. After that, key rotation, reconnection, and OTA updates happen without operator involvement.

---

## Device Identity

Every node (hardware Compute Node, Soft Node, Virtual Node, Room Mic) has a unique **WireGuard keypair** generated at first boot or provisioning.

- The **WireGuard public key** is the canonical device identity. It is used everywhere a device ID is needed.
- The **private key** never leaves the device. It is stored in a protected location (encrypted filesystem partition on Linux SBCs, secure element or flash write-protect on Teensy, OS keychain on Soft Node).
- WireGuard uses Curve25519 for key exchange and ChaCha20-Poly1305 for authenticated encryption.

A device fingerprint for display purposes is the first 8 characters of the base64-encoded public key, e.g. `ozma:wK3fP2aX`.

---

## WireGuard Mesh

All Ozma traffic — HID reports, audio, video, control plane — is carried inside a **WireGuard VPN mesh** that overlays whatever physical network is present.

```
Physical network (LAN, WiFi, internet, 4G, VPN — anything)
        │
        │  WireGuard tunnel (UDP, port 51820)
        │  Curve25519 key exchange
        │  ChaCha20-Poly1305 encryption
        │
  Ozma virtual network  (10.200.0.0/16)
        │
   All existing protocols run here, unchanged
   (HID :7331, VBAN :6980, REST :7380, etc.)
```

### Virtual Network Addressing

| Range | Role |
|---|---|
| `10.200.0.1` | Controller |
| `10.200.1.0/24` | Hardware Compute Nodes |
| `10.200.2.0/24` | Room Mic devices |
| `10.200.3.0/24` | Soft Nodes |
| `10.200.4.0/24` | Virtual Nodes |
| `10.200.100.0/24` | Relay servers |

Addresses are assigned by the Controller during enrollment and are stable for the lifetime of the device registration.

### Deployment Modes

**Mode 1: Isolated LAN (no internet)**
- No relay server. WireGuard runs on the LAN.
- mDNS works because all devices are on the same physical subnet.

**Mode 2: Direct Internet**
- Controller is internet-reachable on UDP 51820.
- Nodes connect by WireGuard to the Controller's public endpoint.
- REST API is only reachable inside the WireGuard overlay — not exposed on the public IP.

**Mode 3: Relay (Ozma Connect)**
- Both Controller and nodes peer with a relay server.
- The relay is a WireGuard L3 forwarder — it does **not** decrypt traffic.
- The relay coordinator authenticates peers using their registered public keys.
- The coordinator protocol is an open specification. Self-hosting is supported.

---

## Enrollment

Enrollment adds a new node's public key to the Controller's authorized peer list and provides the node with the WireGuard configuration needed to connect.

### First-Boot Sequence

1. Node generates a WireGuard keypair and stores the private key in protected storage.
2. Node enters **enrollment mode**: broadcasts a beacon or displays its public key fingerprint.
3. Node sends an enrollment request to the Controller.

### Enrollment Request

`POST http://<controller>:7380/api/v1/enroll`

```json
{
  "public_key": "<base64-encoded Curve25519 public key>",
  "hw": "milkv-duo-s",
  "fw": "0.2.0",
  "caps": ["hid", "audio"],
  "fingerprint": "wK3fP2aX"
}
```

### Approval

- The Controller places the request in a **pending** queue.
- An admin approves or rejects via the web UI or REST API.
- On approval, the Controller assigns a stable `10.200.x.x` virtual IP and adds the node as a WireGuard peer.

### Enrollment Response (200 OK)

```json
{
  "status": "approved",
  "assigned_ip": "10.200.1.12/16",
  "controller_public_key": "<base64>",
  "controller_endpoint": "203.0.113.5:51820",
  "relay_endpoint": "relay1.ozma.io:51820",
  "relay_public_key": "<base64>"
}
```

### Revocation

`DELETE /api/v1/nodes/{id}` removes the node's public key from the WireGuard peer list. The node is immediately unable to establish a tunnel. No explicit revocation message is needed.

---

## Control Plane Authentication

### Device Clients (Nodes → Controller)

Node-to-Controller requests are authenticated by the WireGuard peer identity alone. The Controller looks up the source WireGuard IP to determine which node is making the request. No additional token required.

### Human / Application Clients (Web UI, CLI, third-party)

Requests from browsers and tools require a **bearer token** (JWT signed with the Controller's Ed25519 key).

- `POST /api/v1/auth/token` with `{ "password": "..." }` returns a signed JWT.
- Default expiry: 24 hours. Configurable.
- Tokens are scoped: `read`, `write`, `admin`.
- All REST endpoints except `/api/v1/enroll` and `/api/v1/auth/token` require a valid token when accessed from outside the WireGuard network.

---

## OTA Firmware Signing

Firmware images are signed with Ed25519. Nodes verify the signature before writing to the inactive partition. An image that fails verification is rejected.

### Firmware Manifest

```json
{
  "version": "0.3.1",
  "platform": "milkv-duo-s",
  "url": "/api/v1/firmware/milkv-duo-s/0.3.1.img.gz",
  "sha256": "e3b0c44...",
  "size_bytes": 41943040,
  "signature": "<base64-encoded Ed25519 signature>",
  "signed_payload": "milkv-duo-s:0.3.1:<sha256>"
}
```

The signature covers `<platform_id>:<version>:<sha256_of_image>`, binding content, version, and target platform together.

### Node Verification Steps

1. Download firmware image.
2. Verify SHA-256 matches `sha256` field.
3. Reconstruct `signed_payload` and verify `signature` using the embedded verification public key.
4. If either check fails: discard image, log error, do not write to flash.
5. If both pass: write to inactive partition, set boot flag, reboot.

### Rollback

If the node fails to check in with the Controller within 5 minutes of rebooting into new firmware, the bootloader reverts to the previously-active partition.

---

## Threat Model

| Threat | Mitigation |
|---|---|
| Eavesdropping on HID / audio / video | WireGuard ChaCha20-Poly1305 encryption on all paths |
| Rogue node injecting HID reports | Node must hold private key for an approved WireGuard peer; enrollment requires admin approval |
| Man-in-the-middle on control plane | WireGuard provides mutual auth; control plane only reachable inside the tunnel |
| Unauthorized firmware install | Ed25519 signature required on all images |
| Firmware downgrade attack | Signed payload includes version string; Controller only distributes current or newer versions |
| Stolen node (physical access) | Private key compromise allows that one node until admin revokes it; does not affect other nodes |
| Relay server compromise | Relay sees only encrypted WireGuard packets; cannot decrypt or inject traffic |
| Enrollment abuse | Rate-limited; requests require admin approval |
| Token theft (web UI) | JWTs are short-lived (24h); WireGuard-sourced requests do not use tokens |
