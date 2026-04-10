# Security Architecture

This document defines the Ozma security model: device identity, enrollment, transport encryption, control plane authentication, and OTA firmware signing.

The previous threat model — "trust the LAN because it is physically isolated" — is replaced here with a cryptographic trust model that works on a local isolated LAN, over a corporate VLAN, over a cloud relay, and across the internet. Physical isolation remains a valid and simpler deployment option, but the protocol no longer requires it.

---

## Guiding Principles

1. **Device identity is a keypair, not a location.** A node is trusted because it holds a private key whose corresponding public key the Controller has approved — not because it is at a particular IP address.
2. **Every data path is encrypted.** No Ozma traffic travels in plaintext outside the Controller host.
3. **Network topology is decoupled from trust.** The same node can be on the local LAN today and behind a 4G modem tomorrow; the security model does not change.
4. **Operator approves enrollment; the system handles the rest.** A human grants a node permission to join once. After that, key rotation, reconnection, and OTA updates happen without operator involvement.

> **Data privacy and government access commitments are in [Privacy Architecture](privacy.md).**
> This document covers the cryptographic security model. Privacy.md covers what
> Connect can see, what it cannot, and what happens when a government asks.

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
| `10.200.5.0/24` | Camera Nodes |
| `10.200.100.0/24` | Relay servers |
| `10.201.0.0/24` | Controller-to-controller peering overlay |
| `10.202.0.0/16` | Mobile app clients (Android + iOS) |

Addresses are assigned by the Controller during enrollment and are stable for the lifetime of the device registration.

Mobile clients occupy a distinct `/16` (`10.202.0.0/16`) that is explicitly **excluded** from the WireGuard bypass check. Requests arriving from mobile addresses are never treated as trusted mesh nodes — they must authenticate via mTLS or JWT regardless of network path.

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

## Device Assurance Levels

Every Ozma endpoint — hardware node, desktop agent, mobile app — is assigned a **device assurance level** at enrollment based on the hardware security capabilities it can demonstrate. This level is stored in the controller's device registry, surfaced in the admin UI, and can be used as a policy gate.

| Level | Name | What it means |
|---|---|---|
| **3** | Hardware-attested | Key generated inside hardware security module; external attestation quote cryptographically verified by controller |
| **2** | Hardware-bound | Key in hardware security module; hardware-bound but no external attestation quote available |
| **1** | Software-protected | Key protected by OS credential store (encrypted at rest, requires OS auth to access) |
| **0** | Software-only | Key in filesystem; protected only by file permissions |

Level 0 is the fallback for hardware that has no security module (containers, old SBCs, embedded targets). Admin approval remains the primary gate at all levels; assurance level provides visibility and enables policy-based restrictions.

### Attestation by Device Type

#### Hardware Nodes (SBCs / MCUs)

SBCs used as compute or camera nodes typically lack a dedicated TPM. Key storage tiers:

| Platform | Key storage | Assurance level |
|---|---|---|
| Any SBC + ATECC608A or SE050 (i2c secure element, ~$1 BOM) | Hardware secure element; key non-extractable | 3 (with attestation certificate) |
| SBC with eFuse-locked boot (U-Boot secure boot, signed firmware) | Flash-protected; firmware chain verified | 1 + firmware attestation flag |
| SBC without secure element | Protected flash partition | 1 |
| Microcontroller (Teensy, RP2040) | Flash (no hardware protection) | 0 |

At enrollment, nodes provide:
- `hw_serial` — manufacturer hardware serial (MAC-derived or SoC ID where available). Weak signal, spoofable, but logged.
- `fw_signature` — Ed25519 signature over firmware image. Verified against the OTA signing key. Proves the firmware has not been tampered with.
- `secure_element_cert` — (future) X.509 certificate from a hardware secure element (ATECC608A / SE050) proving the WireGuard private key lives in hardware.

#### Desktop Agents (Windows, macOS, Linux)

Desktop agents detect available hardware security on first run and generate their identity keypair accordingly.

**Windows:**
- If TPM 2.0 is available: generate the agent keypair inside the TPM using the Windows CNG `NCrypt` API (`NCRYPT_TPM_PAD_PSS_IGNORE_SALT`). At enrollment, provide a **TPM attestation quote** — a `TPM2_Quote` over the PCR values and agent public key, signed by the TPM's Endorsement Key. The controller verifies the EK certificate chain (from the TPM manufacturer CA) and the quote. This proves the key is hardware-bound and the device's boot state at enrollment time. **Assurance level 3.**
- If no TPM (old hardware, VMs without vTPM): generate key encrypted with Windows DPAPI (`CryptProtectData`). Key is protected by the user's Windows credential and cannot easily be extracted by another user or offline. **Assurance level 1.**

**macOS:**
- Apple Silicon (M1/M2/M3) and Intel Macs with T2 chip: generate keypair in the **Secure Enclave** using `SecKeyCreateRandomKey` with `kSecAttrTokenIDSecureEnclave`. On macOS 14 (Sonoma)+, request **Managed Device Attestation** via the ACME protocol — the Secure Enclave signs an attestation statement that the key is hardware-bound, signed by Apple's attestation CA. The controller verifies the Apple attestation chain. **Assurance level 3 (macOS 14+) or 2 (earlier macOS with Secure Enclave but no ACME).**
- Intel Macs without T2: generate keypair in the macOS Keychain without hardware backing. **Assurance level 1.**

**Linux:**
- If TPM 2.0 is detected (via `tpm2-tools`): generate agent keypair under TPM control. Provide a TPM attestation quote at enrollment as for Windows. **Assurance level 3.**
- If no TPM but Linux kernel keyring available: store key in kernel keyring (protected by login credential). **Assurance level 1.**
- In containers or VMs without vTPM: plaintext key, file permission protected only. **Assurance level 0.** vTPM support in Proxmox and Hyper-V elevates this to level 3 when enabled.

#### Mobile (Android / iOS)

See the [Mobile Client Authentication](#mobile-client-authentication) section. Short summary:
- Android Key Attestation + Play Integrity → **level 3** (StrongBox) or **level 2** (TEE-backed, no StrongBox)
- iOS App Attest (Secure Enclave) → **level 3**
- Software-backed fallback → **level 1**

---

## Enrollment

Enrollment adds a new device's public key to the Controller's authorized peer list and provides the device with the WireGuard configuration needed to join the mesh. All device types — hardware nodes, desktop agents, and mobile clients — use the same enrollment endpoint with type-specific attestation fields.

### Enrollment Request

`POST http://<controller>:7380/api/v1/enroll`

```json
{
  "public_key": "<base64-encoded Curve25519 public key>",
  "device_type": "node",
  "hw": "milkv-duo-s",
  "fw": "0.2.0",
  "caps": ["hid", "audio"],
  "fingerprint": "wK3fP2aX",
  "attestation": {
    "type": "fw_signed",
    "fw_signature": "<base64 Ed25519 signature over firmware image>",
    "hw_serial": "CV1800B-a3f29b1c"
  }
}
```

The `attestation` object is type-specific:

| Device type | `attestation.type` | Attestation payload |
|---|---|---|
| Hardware node (no secure element) | `fw_signed` | `fw_signature` (Ed25519 over firmware), `hw_serial` |
| Hardware node (with secure element) | `secure_element` | `se_cert` (X.509 cert from ATECC608A/SE050), `hw_serial` |
| Windows agent (TPM 2.0) | `tpm2_quote` | `ek_cert` (EK certificate chain), `quote` (TPM2_Quote), `quote_sig` |
| Windows agent (no TPM) | `dpapi_protected` | `os_version`, `hw_serial` |
| macOS agent (ACME, macOS 14+) | `managed_device_attest` | `acme_cert_chain` (Apple attestation CA chain), `hw_serial` |
| macOS agent (Secure Enclave, pre-14) | `secure_enclave` | `hw_serial` |
| macOS agent (no T2/SE) | `keychain_protected` | `os_version`, `hw_serial` |
| Linux agent (TPM 2.0) | `tpm2_quote` | `ek_cert`, `quote`, `quote_sig` |
| Linux agent (no TPM) | `software_protected` | `os_version` |
| Mobile Android | `android_key_attest` | `key_attestation_chain`, `play_integrity_token` |
| Mobile iOS | `app_attest` | `app_attest_statement`, `receipt` |

Missing or absent `attestation` fields are accepted but logged. The resulting assurance level is recorded in the device registry and reflected in the admin UI.

### Approval

- The Controller places the request in a **pending** queue with the resolved assurance level displayed.
- An admin approves or rejects via the web UI or REST API.
- On approval, the Controller assigns a stable virtual IP and adds the device as a WireGuard peer.

### Enrollment Response (200 OK)

```json
{
  "status": "approved",
  "assigned_ip": "10.200.1.12/16",
  "controller_public_key": "<base64>",
  "controller_endpoint": "203.0.113.5:51820",
  "relay_endpoint": "relay1.ozma.io:51820",
  "relay_public_key": "<base64>",
  "assurance_level": 3
}
```

### Assurance Level Policy

Admins can configure minimum assurance level requirements per resource:

- **Minimum enrollment level**: devices below this level are rejected at enrollment (not just flagged). Default: 0 (accept all, flag in UI).
- **Minimum level for write access**: devices below this level get `read` scope only. Useful for enforcing that mutating operations (scenario switches, HID, power control) require at minimum a software-protected key.
- **Minimum level for admin scope**: default: 1. Prevents zero-protection devices from performing admin operations.

These policies are configurable per controller via `PUT /api/v1/security/assurance-policy`.

### Revocation

`DELETE /api/v1/nodes/{id}` removes the device's public key from the WireGuard peer list. The device is immediately unable to establish a tunnel. For mobile clients, additionally adds the mTLS certificate serial to the revocation blocklist (see [Mobile Client Authentication](#mobile-client-authentication)).

---

## Mobile Client Authentication

The Ozma app (Android + iOS) authenticates to controller services without a system-wide VPN. This is required because a system-wide VPN disrupts Android Auto and CarPlay.

### Architecture

Two complementary mechanisms work together:

**WireGuard split tunnel** — the app runs a WireGuard tunnel restricted to Ozma service traffic. All other device traffic (including Android Auto / CarPlay) uses the default network path.

- **Android**: per-app tunnel using `VpnService`. Android Auto (`com.google.android.projection.gearhead`) is explicitly excluded via `addDisallowedApplication()`. All other installed apps can be added as needed.
- **iOS**: route-based tunnel using `NEPacketTunnelProvider` (requires the Network Extension entitlement from Apple). Allowed IPs are scoped to Ozma service address ranges. CarPlay traffic never matches the tunnel routes and is unaffected.

**mTLS client certificates** — the Ozma app presents a hardware-backed client certificate on every HTTPS connection to first-party services. Third-party apps (Jellyfin, Immich, etc.) are protected by WireGuard only and do not use mTLS.

### Certificate Authority Separation

Mobile client certificates are signed by a dedicated **Mobile Client CA** — a separate intermediate CA derived from the controller's root, distinct from the Mesh CA used for node identities. This separation ensures:

- A compromised Mesh CA cannot forge mobile client credentials
- A compromised Mobile Client CA cannot forge node identities
- Revocation lists and cert stores for each trust domain remain independent

### Hardware-Backed Key Storage

| Platform | Mechanism | Key types supported | Extractable? |
|---|---|---|---|
| Android (StrongBox) | Dedicated secure element (Titan M, NXP SE050) | P-256, RSA | No |
| Android (TEE-backed) | ARM TrustZone in main SoC | P-256, RSA, X25519 (API 31+) | No |
| Android (software) | Android userspace | All | Yes (if rooted) |
| iOS (Secure Enclave) | Dedicated secure element, all iPhone 5s+ | P-256 only | No |

**mTLS keypair**: generated natively in Keystore / Secure Enclave (P-256 / ECDSA). Private key is non-extractable on all hardware-backed tiers.

**WireGuard keypair**: uses X25519. On Android 12+ (API 31), this can be generated directly in Keystore. On older Android and all iOS, generate the keypair in software and encrypt it with a hardware-backed AES-256-GCM key that lives in Keystore / Secure Enclave — the WireGuard private key never touches disk unencrypted, and decryption requires device authentication.

The key backing tier is recorded at enrollment and surfaced in the admin UI per device.

### Enrollment Flow

Enrollment provisions both credentials in a single step:

1. Admin generates an enrollment QR code from the controller dashboard. The QR encodes:
   ```
   ozma-enroll://v1/<token>/<controller_fingerprint>/<endpoint>
   ```
   The token is **single-use**, **160-bit entropy** (`secrets.token_bytes(20)`), and expires after **10 minutes**. The controller fingerprint allows the app to verify the enrollment response before trusting it.

2. User scans QR in the Ozma app. The app:
   - Generates a WireGuard keypair (X25519) and an mTLS keypair (P-256) in hardware-backed storage
   - Requests **Android Key Attestation** (or **App Attest** on iOS) on the mTLS keypair — proof that the key was generated on a real device inside hardware-backed storage, verifiable without Google/Apple servers after initial registration
   - Sends `POST /api/v1/mobile/enroll` with both public keys, the enrollment token, and the attestation token

3. Controller validates:
   - Enrollment token is valid, unused, and not expired (marks it consumed immediately)
   - Play Integrity / App Attest verdict (recorded; enforcement is admin-configurable)
   - Android Key Attestation chain (verifies the mTLS key is hardware-backed)

4. Controller responds with:
   - WireGuard peer config (controller public key, allowed IPs, endpoint)
   - Signed mTLS certificate (P-256, 30-day lifetime, `CN=<user_id>:<device_id>`)
   - Assigned IP in `10.202.0.0/16`

5. App activates the WireGuard tunnel and stores the mTLS cert. The enrollment QR is now invalid.

### Certificate Lifecycle

- **Lifetime**: 30 days
- **Renewal**: background task begins renewal at day 15. Uses `WorkManager` (Android) or `BGTaskScheduler` (iOS) with `RequiresNetworkConnectivity`. Exponential backoff on failure.
- **Expiry warnings**: in-app notification at 7 days remaining; persistent notification at 3 days; access degraded at expiry
- **Renewal authentication**: the renewal request (`POST /api/v1/mobile/renew`) is authenticated with the existing mTLS cert — no user interaction required

### Attestation

| Mechanism | Platform | What it proves | When checked |
|---|---|---|---|
| Android Key Attestation | Android | mTLS private key is hardware-backed, generated on this device | At enrollment (key attestation chain verified server-side) |
| App Attest | iOS | mTLS key was generated inside this device's Secure Enclave by your genuine app | At enrollment |
| Play Integrity | Android | Device passes basic Android CDD integrity checks; app is genuine | At enrollment (verdict recorded) |

Attestation is recorded in the enrollment record and surfaced in the admin UI. Failed or missing attestation logs a security event but does not hard-reject enrollment by default. Admins can enable `require_device_integrity` policy to gate enrollment on passing attestation.

### Device Management

```
GET    /api/v1/mobile/devices              — list enrolled devices for current user (admin: all users)
GET    /api/v1/mobile/devices/{id}         — device detail: user, enrollment time, last seen, attestation, key backing tier
DELETE /api/v1/mobile/devices/{id}         — revoke (user: own devices; admin: any)
POST   /api/v1/mobile/enroll              — enrollment (QR token required; unauthenticated)
POST   /api/v1/mobile/renew               — cert renewal (mTLS-authenticated)
```

**Revocation** is immediate on `DELETE`:
1. WireGuard peer removed — new handshakes fail within seconds
2. mTLS certificate serial added to blocklist — rejected on next connection (TLS session resumption is disabled for mTLS-authenticated connections)
3. Any active relay sessions for the device are dropped

**Per-user device limit**: default 10 enrolled devices. Enrollment beyond the limit requires revoking an existing device. Configurable by admin.

**Last-seen tracking**: the controller records the timestamp of the last successful mTLS authentication per device. Devices inactive for 30+ days are flagged for review in the admin UI.

### TLS Session Security

TLS session resumption is **disabled** for connections requiring mTLS client certificates. This ensures the revocation blocklist is checked on every connection, not just on initial handshake. A revoked certificate is rejected immediately on the next request regardless of any prior session state.

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

## DNS Integrity Verification

DNS is a foundational component of almost every network operation. Compromised DNS
can redirect users to malicious sites even when every other security control is in
place. Ozma actively verifies that DNS is operating correctly and has not been
tampered with.

### Checks performed

| Check | What it detects | How |
|---|---|---|
| Resolver integrity | System resolver returns different IP than DoH reference | Compare getaddrinfo() vs Cloudflare/Google DoH for a stable domain |
| Transparent interception | ISP proxying port 53 without disclosure | Query `one.one.one.one` — system resolver must return Cloudflare's well-known IPs |
| NXDOMAIN hijacking | ISP returning a real IP for non-existent domains ("search assist") | Query an IANA-reserved `.invalid` TLD — must return NXDOMAIN |
| DNSSEC validation | Resolver validates signatures; forged records return SERVFAIL | Query known-valid and known-broken signed zones via DoH with validation enabled |
| DNS rebinding guard | Public name resolves to private/RFC-1918 address (SSRF / LAN pivot) | Every proxy request's resolved IP is checked before forwarding; non-allowlisted private IPs are blocked |
| Captive portal | Network has intercepted HTTP (hotel WiFi, airport, corporate MITM) | Fetch canary URL and verify expected response; redirects or unexpected content indicate a captive portal |
| DNS leak (VPN mode) | DNS queries escape the tunnel in full-tunnel VPN mode | Verify queries route through the exit resolver when VPN is active |

### DNS rebinding protection

The service proxy includes a `DNSRebindingGuard` that rejects any request where
a public hostname resolves to a private IP range (RFC 1918, CGNAT, link-local,
loopback). This prevents:

- Browser-based LAN pivoting via WebRTC / fetch requests
- SSRF attacks through Ozma's proxy layer
- Domain name squatting that routes to internal infrastructure

Private-range addresses can be explicitly allow-listed for legitimate local services:

```
POST /api/v1/dns/rebinding/allowlist
{ "entries": ["controller.local", "192.168.1.100"] }
```

### Continuous monitoring

`DNSVerifier` runs a full check suite on controller startup (after a 15-second
delay) and every 5 minutes thereafter. When issues are detected, re-check interval
drops to 60 seconds. Results are surfaced in the dashboard and can be queried via
the API:

```
GET  /api/v1/dns/integrity           — controller's current assessment
GET  /api/v1/dns/integrity/all       — all nodes + controller
POST /api/v1/dns/integrity/run       — trigger immediate check
POST /api/v1/dns/environment         — node/agent submits its own assessment
GET  /api/v1/dns/environment/{id}    — specific node's latest assessment
```

Nodes and agents can run the same checks locally and submit results via
`POST /api/v1/dns/environment`. A node on a network with NXDOMAIN hijacking will
flag this independently of the controller, giving per-node DNS visibility.

---

## Threat Model

| Threat | Mitigation |
|---|---|
| Eavesdropping on HID / audio / video | WireGuard ChaCha20-Poly1305 encryption on all paths |
| Rogue node injecting HID reports | Node must hold private key for an approved WireGuard peer; enrollment requires admin approval |
| Man-in-the-middle on control plane | WireGuard provides mutual auth; control plane only reachable inside the tunnel |
| Unauthorized firmware install | Ed25519 signature required on all images |
| Firmware downgrade attack | Signed payload includes version string; Controller only distributes current or newer versions |
| Stolen node (physical access) | Private key compromise allows that one node until admin revoked; assurance level 3 nodes have non-extractable keys (secure element) — physical theft does not yield the private key |
| Relay server compromise | Relay sees only encrypted WireGuard packets; cannot decrypt or inject traffic |
| Enrollment abuse | Rate-limited; requests require admin approval; mobile enrollment tokens are single-use and 10-minute TTL |
| Token theft (web UI) | JWTs are short-lived (24h, 2–4h for mobile); WireGuard-sourced mesh node requests do not use tokens |
| SSRF via service proxy | Target host validated against blocklist; cloud metadata ranges blocked |
| Open redirect via login | redirect_to validated as relative path only; absolute URLs rejected |
| XSS in login page | All user-controlled values HTML-escaped before rendering |
| Password brute force | Argon2id slows attempts; per-user session caps limit session accumulation |
| Cross-user impersonation | Grantor always set from auth context; body parameters ignored |
| Shared service token leakage | Proxy strips Authorization/Cookie headers before forwarding to backends |
| IdP session hijack | Cookies: httponly + SameSite=lax + secure (HTTPS); sessions expire after 24h |
| Public service exposure | Requires admin scope + explicit confirmation; dashboard warning displayed |
| Lost / stolen phone | WireGuard peer + mTLS cert revoked immediately on DELETE /api/v1/mobile/devices/{id}; hardware-backed keys non-extractable from locked device; TLS session resumption disabled so blocklist is checked on next connection |
| Phone compromised (software exploit) | mTLS private key non-extractable from Keystore/Secure Enclave even with root (TEE/hardware boundary); WireGuard key protected by hardware-backed AES wrap; 30-day cert expiry limits window without active revocation |
| Mobile enrollment QR code theft | Token is single-use and expires in 10 minutes; controller fingerprint in QR binds enrollment response to specific controller; audit log records all attempts including failures |
| Mobile client inheriting mesh node trust | Mobile clients assigned 10.202.0.0/16, excluded from wireguard_bypass_subnets; always require mTLS or JWT |
| Fake Ozma app on attacker device | Android Key Attestation / App Attest proves key generated on genuine hardware by genuine app; Play Integrity confirms device integrity — all checked at enrollment |
| Third-party app access (Jellyfin etc.) without mTLS | Protected by WireGuard tunnel only; no mTLS — security bounded by WireGuard key storage quality (TEE-wrapped on hardware-backed devices) |
| DNS rebinding attack (browser pivot to LAN) | DNSRebindingGuard rejects proxy requests where a public name resolves to a private/RFC-1918 address; allowlist required for any legitimate private-range endpoint |
| Transparent DNS interception (ISP port-53 proxy) | DNSVerifier compares system resolver against DoH reference for stable canary names; mismatch flagged and surfaced in dashboard |
| NXDOMAIN hijacking (ISP search assist) | DNSVerifier queries an IANA-reserved .invalid TLD; any non-NXDOMAIN response is flagged as NXDOMAIN hijacking |
| Captive portal (hotel / airport WiFi) | Captive portal detection run on startup and periodically; detected portals surface as warnings — VPN or mTLS operations will not work until portal is cleared |
| DNSSEC validation bypass | DoH resolver with DNSSEC validation checks known-valid and known-broken signed zones; resolver that doesn't validate is flagged |
| DNS leak in VPN full-tunnel mode | DNS leak check verifies queries route through tunnel resolver in full-tunnel VPN mode |
