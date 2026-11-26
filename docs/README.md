# Ozma Documentation

Technical documentation for the Ozma software-defined KVM fabric.

## Contents

- [Architecture](architecture.md) — system overview, three-layer model, components, data paths
- [Protocols](protocols.md) — wire protocol specifications, packet formats, port assignments
- [Security](security.md) — device identity, WireGuard mesh, enrollment, OTA signing, threat model
- [Getting Started](getting-started.md) — prerequisites, dev harness setup, first switch
- [Aperture policy plan](aperture-policy.md) — capability and security negotiation model, grants, consent, and revocation
- [GUI configuration alpha plan](gui-configuration-alpha-plan.md) — intent-led setup, progressive configuration detail, tray integration, and release gates
- [GUI configuration alpha guide](gui-configuration-alpha.md) — current Windows tray and setup journey, build instructions, and supported limits
- [Video over USB plan](usb-video-plan.md) — virtual display sink, UVC camera, and raw Ozma USB link design
- [Rust reconciliation plan](rust-reconciliation-plan.md) — replacing the Python node stack with `ozma-node` profiles, providers, and parity gates
- [Multi-seat plan](multi-seat-plan.md) — multiple users on one physical PC, each seat exposed as an Ozma node
- [Rust multi-seat foundation](multiseat-rust.md) — planning crate (`ozma-multiseat`), current scope, and remaining node/OS integration
- [Node ↔ Hermes parity plan](node-hermes-parity.md) — Rust node feature parity, remote displays, session/media architecture, UX, roadmap
- [Remote scene graph integration plan](remote-scene-integration.md) — native presentation, media resources, adaptive rendering, and compatibility
- [Protobuf-core protocol plan](protobuf-core-plan.md) — protobuf negotiation/meta contract, JSON display definitions, field tables, and compact updates
- [Display device adapters](display-device-adapters.md) — browser, SBC/e-ink, MCU, Stream Deck, MIDI/OSC, RGB, and native adapter guidance
- [Display widget and theme library research](display-widget-library-research.md) — vendorable widget/chart libraries, theme sources, licensing, and safe theme-pack design
- [Display platform execution plan](display-platform-execution-plan.md) — phased tasks for the lossless Widget IR, asset repository/shared database, fonts, renderers, and importers
- [Software source management plan](software-source-management-plan.md) — Steam/Microsoft Store/native package sources, inventory, reporting, and update-state model
- [Parity matrix](parity-matrix.md) — Python → Rust parity gates, current evidence, and gaps
- [Plan execution status](plan-execution-status.md) — what is merged and tested so far
- [Phase 4 cutover audit](phase4-cutover-audit.md) — current Python→Rust cutover matrix, blockers, deletion criteria, rollback tags, and staged plan (2026-11-25; Phase 4 not complete)

## Protocol Specifications

Detailed protocol specs live in the `protocol/` submodule under `protocol/specs/`.
