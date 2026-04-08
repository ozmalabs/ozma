# Activation Time and Pipeline Warmth

**Status:** Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

Steady-state latency and activation time are fundamentally different concerns.
This document specifies how the routing protocol tracks activation time for
links and devices, how pipeline activation time is computed from individual hop
times, and how pipelines MAY be kept warm to reduce switching latency at the
cost of additional resource consumption.

## Specification

### Activation Time

Steady-state latency (how long each packet/frame takes to traverse a link) is
fundamentally different from activation time (how long it takes to bring a link
from cold to flowing). The routing protocol MUST track both separately because
they affect different decisions:

- **Latency** determines whether a pipeline can satisfy an intent's real-time
  requirements once running.
- **Activation time** determines how quickly the user experiences a response
  when switching scenarios, and whether pipelines SHOULD be kept warm.

Every link and every device MUST have activation time properties describing how
long state transitions take:

```yaml
ActivationTimeSpec:
  cold_to_warm_ms: float        # initialise: start process, negotiate, handshake
  warm_to_active_ms: float      # begin data flow from ready state
  active_to_warm_ms: float      # stop data but keep initialised
  warm_to_standby_ms: float     # tear down, release resources
  quality: InfoQuality          # provenance of these estimates
```

**Examples of activation time**:

| Component | cold->warm | warm->active | Notes |
|-----------|-----------|-------------|-------|
| UDP socket | ~0ms | ~0ms | Already bound, just start sending |
| ffmpeg process | 1000-3000ms | ~0ms | Process start + codec init + first keyframe |
| ffmpeg (already running) | 0ms | ~0ms | Warm -- just start reading frames |
| HDMI capture card | 100-500ms | ~0ms | Signal lock + EDID negotiation |
| KVM switch (HDMI) | 1000-3000ms | ~0ms | Input switch + HDCP re-auth + EDID |
| HDMI matrix (fast) | 200-500ms | ~0ms | Input switch, minimal re-negotiation |
| WireGuard tunnel | 50-200ms | ~0ms | Handshake (already have keys) |
| Connect relay | 200-1000ms | ~0ms | Relay allocation + WireGuard through relay |
| PipeWire link | ~0ms | ~0ms | Kernel-level, near-instant |
| Sunshine session | 2000-5000ms | ~0ms | RTSP + HDCP + codec negotiation |
| USB gadget (configfs) | 500-2000ms | ~0ms | Gadget creation + host enumeration |
| Manual KVM switch | inf (user action) | ~0ms | Unpredictable -- user must act |

Pipeline activation time MUST be computed from hop activation times. Hops that
have no dependencies between them SHOULD be initialised in parallel. The
pipeline's activation time is determined by its **slowest cold hop**, not the
sum, except where serial dependencies exist:

```
pipeline_activation_ms = max(cold_hop_activation_times)  # parallel init
                       + sum(serial_dependency_times)     # where hop N needs hop N-1 first
```

For example: switching to a scenario that goes through an HDMI matrix (2s) and
needs ffmpeg (3s) -- if they can init in parallel, activation is ~3s, not 5s.
But if ffmpeg cannot start until the matrix has switched (because it needs the
video signal), activation is 2s + 3s = 5s.

### Pipeline Warmth

To avoid activation time on scenario switches, pipelines MAY be kept **warm** --
initialised and ready but not actively flowing data. This trades resources
(running processes, open connections, memory) for faster switching. Warm
pipelines SHOULD be pre-computed for scenarios the user is likely to switch to.

```yaml
WarmthPolicy:
  keep_warm: bool               # should this pipeline be kept warm when not active?
  warm_priority: uint           # if resources are limited, which warm pipelines to keep
  max_warm_duration_s: uint?    # auto-cool after this long (null = indefinite)
  warm_cost: WarmCost           # resource cost of keeping this pipeline warm
```

**WarmCost** (informational -- helps the router decide what to keep warm):

```yaml
WarmCost:
  cpu_percent: float?           # estimated CPU usage while warm (idle process)
  memory_mb: float?             # estimated memory usage while warm
  bandwidth_bps: uint64?        # any keepalive traffic
  gpu_slots: uint?              # hardware codec sessions held open
  description: string?          # human-readable cost summary
```

### Who Decides What Stays Warm

1. **Scenario-driven**: The router SHOULD keep pipelines warm for scenarios the
   user is likely to switch to. If the user has two scenarios (Desktop and
   Gaming), the router SHOULD keep the inactive one's video pipeline warm
   (ffmpeg running, reading frames, discarding them -- ready to stream instantly
   on switch).

2. **Intent-driven**: Intents MAY declare activation time constraints:

```yaml
Constraints:
  max_activation_time_ms: float?  # pipeline must activate within this time
```

If an intent specifies `max_activation_time_ms`, the router MUST only select
pipelines that are already warm or have cold activation within the specified
threshold. This forces pre-warming.

3. **Explicit**: The user or automation MAY explicitly mark pipelines to keep
   warm via the API:

```
POST /api/v1/routing/pipelines/{id}/warm    # warm this pipeline
POST /api/v1/routing/pipelines/{id}/cool    # release warm resources
GET  /api/v1/routing/pipelines/warm         # list all warm pipelines with costs
```

4. **Automatic**: The router MAY auto-warm pipelines based on usage patterns.
   If the user switches between two scenarios frequently, the router SHOULD
   learn to keep both warm. If a pipeline has not been used in hours, it SHOULD
   cool automatically (governed by `max_warm_duration_s`).

### Warming Strategies by Component

| Component | How to keep warm | Cost |
|-----------|-----------------|------|
| ffmpeg | Start process, decode to /dev/null | CPU + memory (~50-200 MB) |
| Capture card | Keep signal locked, discard frames | Minimal (hardware idle) |
| HDMI matrix | No warmth concept -- switching is the cost | N/A |
| KVM switch | Pre-switch during warm phase (if writable) | Disrupts current user briefly |
| WireGuard | Keep tunnel established, send keepalives | ~1 KB/s keepalive |
| Connect relay | Keep relay session allocated | ~1 KB/s keepalive |
| Sunshine | Keep RTSP session, pause stream | Memory + GPU session slot |
| UDP transport | Keep socket bound | Negligible |

### KVM Switch Warming

External switches present a special case. You cannot "warm" a switch without
actually switching it. For KVM switches with HDCP negotiation delay, the
activation time is unavoidable. The router MUST account for this honestly -- if
the pipeline goes through a slow switch, the activation time MUST be reported
as-is. The user MAY mitigate this by using a faster switch, removing the switch
from the path, or accepting the delay.

For HDMI matrices with multiple outputs, the router MAY pre-route an unused
output to the target input -- warming the HDCP handshake on a different output
port -- but this depends on the matrix having spare outputs.
