# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in ozma, please report it responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Email: security@ozma.dev

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if you have one)

We will acknowledge receipt within 48 hours and provide an initial assessment
within 7 days.

## Security Architecture

The controller implements:
- **API authentication**: JWT bearer tokens signed with the controller's Ed25519 identity key. See [docs/security.md](docs/security.md).
- **WireGuard mesh encryption**: All node traffic encrypted with ChaCha20-Poly1305 inside a WireGuard VPN overlay.
- **Device enrollment**: X25519 key exchange with human approval required for new nodes.
- **Agent action approval**: Configurable approval modes (auto/notify/approve) for AI agent actions.
- **Audit logging**: Tamper-evident hashchained event log (enabled by default).

See [TRUST.md](TRUST.md) for data collection policy and transparency details.

## Scope

Security issues in the following components are in scope:
- Controller REST API and WebSocket server
- Node UDP listener and HID forwarding
- Web UI (XSS, CSRF, injection)
- Authentication and access control
- Audit logging integrity
- Camera privacy framework
- Provisioning and remote access (Onboarding plugin)

## Disclosure Policy

- We will work with you to understand and address the issue
- We will credit you in the security advisory (unless you prefer anonymity)
- We aim to release a fix within 30 days of confirmed vulnerabilities
- We will not pursue legal action against good-faith security researchers
