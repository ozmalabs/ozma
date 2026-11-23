# Ozma Documentation

Technical documentation for the Ozma software-defined KVM fabric.

## Contents

- [Architecture](architecture.md) — system overview, three-layer model, components, data paths
- [Protocols](protocols.md) — wire protocol specifications, packet formats, port assignments
- [Security](security.md) — device identity, WireGuard mesh, enrollment, OTA signing, threat model
- [Getting Started](getting-started.md) — prerequisites, dev harness setup, first switch
- [Aperture policy plan](aperture-policy.md) — capability and security negotiation model, grants, consent, and revocation
- [Video over USB plan](usb-video-plan.md) — virtual display sink, UVC camera, and raw Ozma USB link design
- [Rust reconciliation plan](rust-reconciliation-plan.md) — replacing the Python node stack with `ozma-node` profiles, providers, and parity gates
- [Multi-seat plan](multi-seat-plan.md) — multiple users on one physical PC, each seat exposed as an Ozma node
- [Node ↔ Hermes parity plan](node-hermes-parity.md) — Rust node feature parity, remote displays, session/media architecture, UX, roadmap
- [Remote scene graph integration plan](remote-scene-integration.md) — native presentation, media resources, adaptive rendering, and compatibility
- [Parity matrix](parity-matrix.md) — Python → Rust parity gates, current evidence, and gaps
- [Plan execution status](plan-execution-status.md) — what is merged and tested so far

## Protocol Specifications

Detailed protocol specs live in the `protocol/` submodule under `protocol/specs/`.
