# Trust & Transparency

## Who we are

Ozma is developed by **Ozma Labs Pty Ltd**, an Australian company. Our code is open source under [AGPL-3.0](COPYING).

## What Ozma collects

**By default: nothing.** The controller runs entirely on your network. No telemetry, no phone-home, no analytics, no usage reporting.

**Audit logging** (enabled by default) writes events to a local hashchained log file. This log never leaves your machine unless you explicitly configure export.

**Ozma Connect** (optional cloud service) collects only what's needed to provide the service:
- Account identity (email, hashed password)
- Controller and node public keys (for relay coordination)
- Connection metadata (timestamps, WireGuard peer IPs)

Ozma Connect **never** collects:
- Screen contents or screenshots
- Keyboard input or HID traffic
- Audio streams
- Clipboard contents
- File contents
- Anything inside the WireGuard tunnel (the relay forwards encrypted packets it cannot read)

Config backups stored in Connect are **zero-knowledge encrypted** — encrypted client-side before upload. Ozma Labs cannot read them.

## Network architecture

All Ozma traffic flows inside a **WireGuard VPN mesh**. The controller, nodes, and agents communicate only through encrypted tunnels (Curve25519 key exchange, ChaCha20-Poly1305 encryption).

When using Ozma Connect's relay service, the relay server is a Layer 3 forwarder that sees only encrypted WireGuard packets. It cannot decrypt, inspect, or modify traffic.

The relay coordinator protocol is an **open specification** — self-hosting is supported. The controller has a `connect.coordinator_url` setting that defaults to Ozma's hosted service but can point at any compliant coordinator.

## API authentication

The controller's REST API requires JWT bearer token authentication. Tokens are signed with the controller's Ed25519 identity key. Requests from within the WireGuard mesh (10.200.x.x) are authenticated by WireGuard peer identity and bypass token requirements.

See [docs/security.md](docs/security.md) for the full security architecture.

## How to verify

1. **Read the code.** Every line of the controller, node, agent, and protocol is in this repository under AGPL-3.0.
2. **Inspect network traffic.** All traffic is WireGuard-encrypted. Use `tcpdump` on any interface — you'll see only UDP 51820.
3. **Check the audit log.** `controller/audit_log.py` records all control plane actions in a tamper-evident hashchain.
4. **Run without Connect.** The controller is fully functional with no cloud account, no internet, no external dependencies.

## License boundaries

| Component | License | Source available |
|-----------|---------|-----------------|
| Controller, nodes, agents | AGPL-3.0 | Yes (this repo) |
| Protocol specifications | AGPL-3.0 | Yes (protocol/ submodule) |
| Ozma Connect cloud service | Proprietary | No |
| Hardware designs (PCB, enclosures) | Proprietary | No |
| Third-party plugins | Any license | Via plugin API |
