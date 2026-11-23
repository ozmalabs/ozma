# Ozma Documentation

Technical documentation for the Ozma software-defined KVM fabric.

## Contents

- [Architecture](architecture.md) — system overview, three-layer model, components, data paths
- [Protocols](protocols.md) — wire protocol specifications, packet formats, port assignments
- [Security](security.md) — device identity, WireGuard mesh, enrollment, OTA signing, threat model
- [Getting Started](getting-started.md) — prerequisites, dev harness setup, first switch
- [Node ↔ Hermes parity plan](node-hermes-parity.md) — Rust node feature parity, remote displays, session/media architecture, UX, roadmap
- [Plan execution status](plan-execution-status.md) — what is merged and tested so far

## Protocol Specifications

Detailed protocol specs live in the `protocol/` submodule under `protocol/specs/`.
