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
- WebSocket connections authenticate via `?token=<jwt>` query parameter.

### Node Machine Class

Each node has a `machine_class` that determines its security behaviour:

| Class | Description | Agent default | Consent | Privacy mode |
|-------|-------------|--------------|---------|--------------|
| `workstation` | Someone may be sitting here | mutating = notify, read = auto | Available (opt-in) | Available |
| `server` | Headless / unattended | all = auto | Never | No-op |
| `kiosk` | Has display, no operator | all = auto | Never | No-op |

Set via `PUT /api/v1/nodes/{id}/machine_class` or during node registration.

### AI Agent Action Approval

AI agents interact with machines via the `ozma_control` MCP tool. Each action has a configurable approval mode:

- **auto**: execute immediately (default for read-only actions; default for all actions on server/kiosk nodes)
- **notify**: execute immediately, fire a WebSocket event and notification (default for mutating actions on workstation nodes)
- **approve**: queue the action and wait for human approval via `POST /api/v1/agent/{action_id}/approve` before executing

Per-action overrides via `PUT /api/v1/agent/config` take precedence over machine class defaults.

### Remote Desktop Consent

Remote desktop sessions (`/api/v1/remote/{node_id}/ws`) are protected by JWT auth. For workstation nodes in multi-user or helpdesk deployments, an additional consent flow can be enabled:

- Session enters `PENDING` state, fires a `remote_desktop.consent_request` event
- Local operator approves or rejects via `POST /api/v1/remote/{session_id}/approve`
- If no response within 60 seconds, the session is denied

Server and kiosk nodes **never** require consent — there's no one to ask. Workstation consent is off by default and must be explicitly enabled.

**Privacy mode**: when enabled, blanks the target machine's physical display via DDC/CI during the remote session. Only meaningful for workstation nodes.

### Audit Logging

All control plane actions are recorded in a tamper-evident hashchained audit log (enabled by default). Events include: authentication attempts, remote desktop sessions, agent actions, scenario switches, power operations.

---

## Multi-User Authentication

### User Model

Users are the identity layer above controllers. The controller supports multiple local user accounts with role-based access:

| Role | Scopes | Description |
|------|--------|-------------|
| `owner` | read, write, admin | Full access — creates/deletes users, manages sharing, configures IdP |
| `member` | read, write | Can use the system, register services, create shares |
| `guest` | read | View-only access |

User passwords are hashed with Argon2id (primary) or PBKDF2-SHA256 with 600k iterations (fallback). Password comparison uses constant-time `hmac.compare_digest()`.

### Identity Provider (IdP)

The controller can run a built-in OIDC-compatible identity provider:

- **Password + social login** (Google, Apple, GitHub via OAuth2)
- **Enterprise federation** with AD/Entra/LDAP
- **Session cookies**: httponly, SameSite=lax, secure flag when HTTPS
- **Per-user session caps**: max 50 sessions per user, 10k globally, with LRU eviction
- **Open redirect protection**: `redirect_to` parameters validated as relative paths only
- **XSS protection**: all user-controlled values HTML-escaped in rendered pages

The IdP coexists with the existing device auth model:
- Auth disabled (default): open API, no sessions
- Auth enabled, IdP disabled: single-admin password + JWT
- Auth enabled, IdP enabled: full multi-user with social login; password + JWT still works for API clients

### JWT Token Model

JWTs are signed with the controller's Ed25519 identity key. Multi-user tokens include the user UUID as the `sub` claim. Legacy tokens use `sub: "admin"` for backward compatibility.

---

## Service Proxy Security

### SSRF Protection

The service proxy validates target hosts against a blocklist of dangerous IP ranges:

- `169.254.0.0/16` (link-local — cloud metadata endpoints)
- `0.0.0.0/8`, `240.0.0.0/4`, `255.255.255.255/32` (reserved/broadcast)
- `224.0.0.0/4` (multicast)
- IPv6 loopback, link-local, ULA

Private RFC1918 ranges (`10.x`, `172.16-31.x`, `192.168.x`) are allowed because those are legitimate LAN services.

Subdomains are validated against a strict regex (`^[a-z0-9][a-z0-9-]{0,61}[a-z0-9]?$`) and a reserved name blocklist (`api`, `auth`, `admin`, `www`, etc.).

### Header Isolation

The reverse proxy strips sensitive headers before forwarding to backend services:

- `Authorization` (prevents JWT leakage to backends)
- `Cookie` (prevents IdP session leakage)
- Existing `X-Forwarded-*` headers (prevents spoofing)

Only safe headers are forwarded. `X-Forwarded-For/Proto/Host` are set from trusted request properties.

### TLS Certificate Management

- Controller generates TLS private key locally — it **never** leaves the controller
- Wildcard certificate (`*.user.c.ozma.dev`) obtained via DNS-01 challenge
- Connect provides DNS-01 coordination (sets `_acme-challenge` TXT record)
- Controller completes ACME exchange with Let's Encrypt directly
- Certificate and key stored at `controller/certs/` (backed up via Connect's encrypted backup)

---

## Sharing Security

### Access Control

- **Grant creation**: only the authenticated user can be the grantor (enforced server-side; body `grantor_user_id` is ignored)
- **Grant viewing**: restricted to the grantor, grantee, or admin
- **Grant revocation**: restricted to the grantor or admin
- **Expiry**: grants can have an expiry timestamp; expired grants are inactive

### Cross-Controller Trust

Controllers link via the existing mesh CA trust model (mutual Ed25519 certificate signing). Cross-user proxy requests carry the grant ID for verification on both sides.

For Connect relay sharing, grant tokens are JWTs signed by Connect, binding grantor, grantee, and service ID. Both controllers validate the token before proxying.

---

## External Publishing Security

- **Private mode** (default): requests authenticated via the user's IdP session
- **Public mode**: requires `admin` scope + explicit `confirm_public: true` to activate. Dashboard displays a warning.
- **Rate limiting**: configurable per-published service
- **Domain allowlisting**: optional email domain restrictions for private-mode access

---

## File Security

Sensitive files are written with restrictive permissions:

| File | Contains | Permissions |
|------|----------|-------------|
| `users.json` | Argon2id password hashes | `0600` |
| `connect_cache.json` | Connect JWT token | `0600` |
| `certs/` | TLS private keys | `0600` |
| `mesh_registry.json` | Mesh CA private key (encrypted) | `0600` |

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
| SSRF via service proxy | Target host validated against blocklist; cloud metadata ranges blocked |
| Open redirect via login | redirect_to validated as relative path only; absolute URLs rejected |
| XSS in login page | All user-controlled values HTML-escaped before rendering |
| Password brute force | Argon2id slows attempts; per-user session caps limit session accumulation |
| Cross-user impersonation | Grantor always set from auth context; body parameters ignored |
| Shared service token leakage | Proxy strips Authorization/Cookie headers before forwarding to backends |
| IdP session hijack | Cookies: httponly + SameSite=lax + secure (HTTPS); sessions expire after 24h |
| Public service exposure | Requires admin scope + explicit confirmation; dashboard warning displayed |
