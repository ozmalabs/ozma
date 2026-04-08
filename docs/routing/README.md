# Ozma Routing Protocol Specification

**Version**: 0.1 (unstable)
**Status**: Draft

---

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
specification are to be interpreted as described in
[RFC 2119](https://www.rfc-editor.org/rfc/rfc2119).

---

## Abstract

Ozma is a software-defined KVMA router. This specification defines how
signals — video, audio, peripherals, data, power, and control — are modelled,
discovered, negotiated, and delivered across an arbitrarily complex graph of
devices, transports, and conversions.

Every path through the system — from a keyboard through a USB cable, across a
network, into a USB gadget on a target machine; from a wall outlet through a
UPS, PDU, and PSU to a CPU core; from a microphone through a mix bus, insert
chain, and network transport to a remote speaker — is a graph of typed ports
connected by links. Each link has measurable properties (bandwidth, latency,
jitter, loss, voltage, current). Each port has discoverable capabilities
(supported formats, maximum throughput, power budget). The router assembles
the best pipeline through this graph to satisfy a declared **intent**.

This specification does not define wire formats or byte layouts — those live in
individual protocol specs (`protocol/specs/`). This specification defines the
**model** that those specs implement.

---

## Scope

The specification models the complete physical and logical infrastructure of
any computing environment — from a single PC on a desk to a multi-hall
datacentre:

- **Signal routing** — video, audio, HID, RGB, screen, control surface I/O as
  pipelines with format negotiation, bandwidth calculation, and latency budgeting
- **Device topology** — every device as a compound entity with ports, internal
  links, and discoverable capabilities
- **Power** — voltage rails traced from utility feed through UPS, PDU, PSU to
  every consumer
- **Physical environment** — sites, buildings, rooms, racks, furniture, floor
  plans with RF propagation modelling
- **Monitoring** — real-time metrics, historical journal, trend analysis,
  asset inventory — by construction, not as a separate system
- **Audio production** — mix buses, monitor controller, insert chains, spatial
  audio, metering, Dante-equivalent clock sync
- **Security** — unified identity and authentication, security posture with
  cascading policy, credential lifecycle, compliance gap reporting
- **Asset management** — hardware identity with serial numbers, firmware
  lifecycle, compatibility checking, build validation

### Scale independence

The same primitives model every scale. No model changes between scales:

| Scale | Example |
|-------|---------|
| Single device | One PC — USB topology, audio routing, storage health, thermal monitoring |
| Desk | PC + monitors + speakers + peripherals — spatial audio, RGB, power budget |
| Room | Multiple desks + rack + AV receiver + cameras — KVM switching, network, AV |
| Building | Multiple rooms + structured cabling + WiFi + IoT — fleet management, VLAN |
| Campus | Multiple buildings + inter-building fiber + WAN — federation, remote access |
| Datacentre | Halls + rack rows + A/B power + CRAC cooling — full infrastructure graph |

### Decentralised decision-making

The specification defines a global model but does not require global knowledge
for every decision. Routing decisions SHOULD be made at the level that has
sufficient context:

- **A node** makes local decisions: USB port recommendations, fan control,
  power budget. It does not need datacentre-level knowledge.
- **A controller** makes mesh-level decisions: pipeline assembly, failover,
  audio routing. It does not need other controllers' internal topology.
- **Connect** makes platform-level decisions: firmware distribution, device
  database popularity, aggregate failure patterns.

The graph is composable, not monolithic. Each participant holds the subgraph
relevant to its decisions. More data enables better decisions, but the system
MUST function at every level of knowledge — degrading gracefully as information
decreases.

### Plugin extensibility

Everything that is not a core graph primitive is a plugin. Third-party plugins
MAY register new transport types, device classes, and capabilities at runtime.
The plugin interface is Python and MUST remain stable across implementation
language changes (e.g., Rust migration via PyO3).

---

## Specification documents

### Core model

| Document | Description |
|----------|-------------|
| [Graph Primitives](graph-primitives.md) | Device, Port, Link, Pipeline — the four core types. REQUIRED reading. |
| [Intents](intents.md) | What the user wants to achieve — constraints, preferences, degradation, composition. |
| [Formats](formats.md) | Video, audio, HID, screen, RGB, control, data formats. Three-phase negotiation. |
| [Information Quality](quality.md) | Seven trust levels, provenance, data freshness, refresh scheduling, temporal decay. |

### Routing engine

| Document | Description |
|----------|-------------|
| [Route Calculation](routing.md) | Cost model, constraint satisfaction, path computation, remediation, intent bindings. |
| [Plugins](plugins.md) | Registration, lifecycle, language guarantee. Transport, device, codec, converter, switch contracts. |
| [Transports](transports.md) | Transport characteristics, Bluetooth, WiFi, serial, constrained transports, multiplexed connections. |
| [Clock Model](clock.md) | PTP, drift compensation, jitter buffers, Dante-equivalent sync, sample-accurate timing. |
| [Topology Discovery](discovery.md) | Five-layer discovery, opaque devices, compound decomposition, calibration probes. |

### Device model

| Document | Description |
|----------|-------------|
| [External Switches](switches.md) | KVM switches, HDMI matrices, controllability, switch matrix, commanded state. |
| [Activation and Warmth](activation.md) | Activation time, pipeline warmth, warm cost, pre-computation. |
| [Device Capacity](capacity.md) | Resource pools, pressure, budgets, adaptive enforcement. |
| [Endpoints](endpoints.md) | Screens, RGB, control surfaces — as pipeline endpoints. |
| [Device Types](devices.md) | Cameras, phones, actuators, sensors, VMs, media receivers, network gear, building management. |

### Physical model

| Document | Description |
|----------|-------------|
| [Power Model](power.md) | Voltage rails, USB PD, PoE, power distribution (PDU, UPS, strips), power connectors. |
| [Physical Environment](environment.md) | Furniture, racks, sites, spaces, zones, floor plans, datacentre model. |
| [Control Path](control-path.md) | How commands reach devices, dependency chains, reachability, fallback paths. |
| [Thermal Management](thermal.md) | Fan curves, power profiles, thermal zones, intent-driven control. |

### Audio

| Document | Description |
|----------|-------------|
| [Audio Routing](audio.md) | Mix buses, monitor controller, insert chains, cue sends, spatial audio, metering, PipeWire integration. |

### Security

| Document | Description |
|----------|-------------|
| [Security](security.md) | Encryption, identity, authentication, credentials, MFA, security posture, incident response. |

### Infrastructure

| Document | Description |
|----------|-------------|
| [Observability](monitoring.md) | Monitoring queries, Sankey diagrams, journal, trend analysis, asset inventory. |
| [Versioning](versioning.md) | Ozma + third-party firmware (LVFS), BIOS/AGESA tracking, fleet updates. |
| [Device Database](device-database.md) | Universal catalog: entry schema, category specs, pinouts, compatibility engine. |
| [Node Definition](node-definition.md) | Node as compound device: platform, USB gadget, peripherals, services, lifecycle. |
| [Implementation Guide](implementation.md) | Incremental adoption, performance, PipeWire integration. Non-normative. |
| [Existing Protocols](compatibility.md) | Relationship to existing Ozma protocol specs. |

### Reference

| Document | Description |
|----------|-------------|
| [Appendices](appendices.md) | Worked examples: gaming scenario switch, format negotiation, intent composition. |

---

## Changelog

| Date | Change |
|------|--------|
| 2026-04-07 | Initial draft — monolithic document |
| 2026-04-08 | Split into sub-documents with RFC 2119 language |
