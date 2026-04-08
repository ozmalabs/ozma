# Implementation Guide

**Status**: Draft
**RFC 2119 Conformance**: The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

---

## Abstract

This document provides implementation guidance for the Ozma routing
specification, covering incremental adoption strategy, performance
considerations, and PipeWire integration with detailed mapping tables.

This document is **non-normative**. It describes how the specification is
expected to be implemented, without constraining the implementation. Where
RFC 2119 keywords appear, they indicate recommendations, not requirements.

---

## Specification

### 1. Incremental Adoption

The routing model is RECOMMENDED to be adopted incrementally over the existing
codebase. The following phased approach is RECOMMENDED:

1. **Phase 1: Graph model** -- Build the graph data structures. Populate them
   from existing discovery (mDNS, V4L2 enumeration, PipeWire node listing). No
   routing changes -- the graph is observational only.

2. **Phase 2: Intent system** -- Define intents. Map existing scenario switching
   to intent-driven pipeline selection. The router recommends pipelines; the
   existing code activates them.

3. **Phase 3: Format negotiation** -- Add capability enumeration to ports. The
   router negotiates formats instead of hardcoding them.

4. **Phase 4: Transport plugins** -- Factor existing transport code (UDP HID,
   VBAN, RTP) into the plugin interface. New transports can be added without
   modifying core routing.

5. **Phase 5: Active measurement** -- Add link probing. Replace `assumed` and
   `spec` quality data with `measured` data. Enable dynamic re-evaluation.

6. **Phase 6: Full routing** -- The router assembles and manages pipelines
   end-to-end. The existing protocol-specific code becomes transport plugins.

### 2. Performance Considerations

- Graph operations (path finding, format negotiation) are RECOMMENDED to
  complete in <1ms for typical graphs (<100 devices, <500 links).
- Pipeline switching (activating a pre-computed pipeline) is RECOMMENDED to
  complete in <10ms for local KVM, <100ms for remote access.
- Measurement probing is RECOMMENDED to not interfere with active pipelines
  (use separate low-priority traffic).
- The graph is RECOMMENDED to be stored in memory, not persisted -- it is
  rebuilt from discovery on each startup.

### 3. PipeWire Integration

On machines running PipeWire, the Ozma routing graph integrates with
PipeWire's graph rather than duplicating it. The two systems have parallel
models -- every Ozma audio concept maps to a PipeWire primitive.

#### 3.1 Node Mapping

| Ozma concept | PipeWire equivalent | Implementation |
|-------------|-------------------|----------------|
| Audio source device | PipeWire node (Audio/Source) | Discovered via `pw-dump` |
| Audio sink device | PipeWire node (Audio/Sink) | Discovered via `pw-dump` |
| Mix bus | PipeWire node with N input port groups | `pw-filter-chain` summing node or `module-null-sink` + volume controls |
| Monitor controller | Combination of `pw-link` + volume + filter-chain | Source selection = link management, dim/mono = filter-chain |
| Insert chain processor | `pw-filter-chain` node | One filter-chain per insert slot, linked in series |
| Room correction EQ | `pw-filter-chain` with biquad filters | `ozma-room-eq` capture -> EQ bands -> playback |
| VBAN network bridge | `pw-cat` virtual source/sink | VBAN receiver -> `pw-cat --playback`; `pw-cat --capture` -> VBAN sender |
| Audio output target | PipeWire module sink | `module-raop-sink`, `module-rtp-sink`, `module-roc-sink`, etc. |
| Delay compensation | `pw-loopback --delay` | Per-output delay alignment |
| Cue send | Mix bus node with independent volume controls | Separate PipeWire node per cue mix |

#### 3.2 Port Mapping

| Ozma concept | PipeWire equivalent |
|-------------|-------------------|
| Audio port (source) | PipeWire output port(s) -- one per channel (FL, FR, etc.) |
| Audio port (sink) | PipeWire input port(s) -- one per channel |
| Channel map | PipeWire port `audio.channel` property |
| Port power budget | N/A in PipeWire (Ozma-only concept) |

#### 3.3 Link Mapping

| Ozma concept | PipeWire equivalent |
|-------------|-------------------|
| Audio link (same machine) | PipeWire Link object between ports |
| Audio link (cross-machine) | VBAN/Opus transport -> `pw-cat` bridge -> PipeWire link |
| Link metrics | PipeWire node `Props` (volume, mute, latency) |
| Format negotiation | PipeWire SPA format enumeration (same-machine); Ozma format negotiation (cross-machine) |

#### 3.4 Routing Modes

1. **pw-link mode** (default): Ozma calls `pw-link` directly to connect
   PipeWire nodes. Simple, works for basic KVM audio. Node-level granularity
   (all channels auto-mapped).

2. **WirePlumber mode** (`OZMA_AUDIO_WIREPLUMBER=1`): Ozma writes a single
   metadata key (`pw-metadata -n ozma set 0 active_node <name>`).
   WirePlumber's Lua script (`ozma-routing.lua`) watches this and manages
   port-level links with explicit channel mapping. More reliable for
   multichannel audio and complex scenarios.

3. **Router mode** (Phase 3+): The routing graph computes audio pipelines
   and translates them to PipeWire operations. WirePlumber executes the
   link commands. The router handles format negotiation, insert chain
   assembly, mix bus creation, and latency compensation. WirePlumber is
   the executor, not the decision-maker.

#### 3.5 Clock Mapping

| Ozma concept | PipeWire equivalent |
|-------------|-------------------|
| Clock domain | PipeWire driver node |
| Clock master | Driver node's hardware clock |
| Sample-accurate sync | PipeWire clock class + rate matching |
| Drift compensation | PipeWire adaptive resampling (built-in) or Ozma-managed `pw-loopback` |

PipeWire already handles same-machine clock synchronisation via its
driver/follower model. Ozma's clock model extends this across machines --
PTP/NTP provide inter-machine sync, and PipeWire handles intra-machine
scheduling. The two complement each other.

#### 3.6 Metering Mapping

PipeWire nodes expose peak levels via the `Props` parameter
(`channelVolumes`, `softVolumes`). For pro audio metering (LUFS, true peak,
VU), a dedicated `pw-filter-chain` analysis node is inserted at each metering
point. The analysis node reads audio data and computes metrics without
modifying the signal (wet_dry = 0.0).

### 4. Responsibility Boundaries

#### 4.1 What PipeWire Handles Natively (Ozma Is RECOMMENDED Not to Duplicate)

- Same-machine buffer management and zero-copy transport
- Hardware device enumeration and driver management
- Format negotiation for same-machine links (SPA format)
- Driver/follower clock scheduling within a machine
- Adaptive resampling for same-machine clock domain mismatches
- Port-level channel routing

#### 4.2 What Ozma Adds on Top of PipeWire

- Cross-machine audio routing (VBAN, Opus, AES67)
- Intent-driven pipeline selection (which sources go where)
- Mix bus and monitor controller as managed virtual devices
- Insert chain orchestration (processor ordering and bypass)
- Cross-machine clock sync (PTP/NTP)
- Power-aware routing (audio device power budgets)
- Monitoring, journaling, and trend analysis on audio paths
- Device database integration (microphone response curves, speaker specs)
