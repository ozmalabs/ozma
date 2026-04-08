# Route Calculation

**Status:** Draft

**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document specifies the route calculation engine for the Ozma routing system.
The router takes the graph (devices, ports, links), a source, a destination, and
an intent, and produces the optimal pipeline. The specification covers the cost
model with spatial zone awareness, constraint satisfaction, path computation with
converter insertion, re-evaluation triggers, pipeline switching and activation,
remediation with safety levels, and intent bindings with condition sources.

---

## Specification

### 1. Cost Model

Each link in the graph has a computed cost. The router MUST find the lowest-cost
path that satisfies all constraints.

**Cost function**:

```
cost(link) = w_latency * latency_ms
           + w_hops * 1
           + w_conversions * (1 if format change else 0)
           + w_bandwidth * (1 - available_bps / required_bps)  # penalise tight fits
           + w_quality_loss * quality_loss_factor
           + w_uncertainty * (1 - trust_factor(info_quality))
           + w_pressure * max(device_pressure(d) for d in devices_on_hop)
```

Where `device_pressure(d)` is the highest resource utilisation ratio across all
resource types on device `d` after accounting for this pipeline's cost:

```
device_pressure(d) = max(
  (d.resource[r].current_usage + pipeline.cost[r]) / d.resource[r].capacity
  for r in resource_types
)
```

This naturally load-balances: a hop through a busy device costs more than the
same hop through an idle one.

Weights (`w_*`) MUST be derived from the intent's preferences:

| Preference | Affects weights |
|-----------|----------------|
| `prefer_lower_latency` | Increases `w_latency` |
| `prefer_fewer_hops` | Increases `w_hops` |
| `prefer_higher_quality` | Increases `w_quality_loss`, decreases `w_conversions` tolerance |
| `prefer_hardware_codec` | Reduces cost of hardware codec hops |
| `prefer_local_zone` | Increases `w_zone` (see Spatial Zones below) |

**Spatial zones** (OPTIONAL cost factor):

Devices MAY be assigned to named spatial zones representing physical areas.
Zones have types that inform routing and automation behaviour:

```yaml
SpatialZone:
  id: string                    # "desk-main", "couch", "conference-table", "server-closet"
  name: string                  # human-readable
  type: ZoneType                # what kind of space this is
  space: string?                # parent space/room ("study", "living_room")
  site: string?                 # parent site ("home", "office-hq")
  devices: DeviceRef[]          # devices in this zone
  furniture: FurnitureRef[]     # furniture in this zone
  adjacent_zones: string[]?     # zone IDs that are physically adjacent
  bounds: ZoneBounds?           # physical extent (for spatial effects, presence detection)

ZoneType: enum
  workstation                   # primary work area (desk, monitors, input devices)
  collaboration                 # meeting/conference area (shared displays, cameras, mics)
  media                         # media consumption area (couch, TV, speakers)
  server                        # infrastructure area (server rack, networking gear)
  utility                       # utility area (printer, storage, supplies)
  common                        # shared/transit area (hallway, kitchen)
  outdoor                       # outdoor area (patio, garden — cameras, sensors, lighting)

ZoneBounds:
  shape: string                 # "rectangle", "polygon", "circle"
  points: Position2d[]?         # floor-plan polygon vertices (for rectangle/polygon)
  center: Position2d?           # for circle
  radius_mm: float?             # for circle
  floor_z_mm: float?            # floor height (for multi-level)
  ceiling_z_mm: float?          # ceiling height

UserZone:
  current_zone: string?         # which zone the user is currently in (null = unknown)
  confidence: InfoQuality       # how we know
  detection_source: string?     # what determined the zone ("active_keyboard", "ble_beacon",
                                # "motion_sensor", "manual", "idle_timeout")
  zone_type: ZoneType?          # type of the current zone (cached for quick routing decisions)
```

The user's current zone is inferred from observable signals — which input
device is active (wired keyboard = desk, wireless keyboard = couch), which
motion sensor fired, or explicit user selection. When a zone is known, the
cost model MUST apply a zone distance penalty:

```
cost(hop) += w_zone * zone_distance(user_zone, device_zone)
```

Where `zone_distance` MUST return 0 for same zone, 1 for adjacent zones, 2+ for
distant zones. This makes the router prefer devices physically near the
user — if the user is at the couch, the TV gets a lower cost than the desk
monitor. Zone definitions and adjacency are user-configured.

The cost model naturally produces the expected behaviour: multi-hop paths
are possible but expensive (each hop adds latency cost + hop cost + potential
conversion cost), so the router avoids them unless no direct path exists.

### 2. Constraint Satisfaction

Before cost ranking, candidate pipelines MUST be filtered by hard constraints.
The router MUST reject any pipeline that violates a hard constraint:

1. **Latency**: Sum of all hop latencies MUST be less than or equal to `max_latency_ms`.
2. **Activation time**: Pipeline activation time MUST be less than or equal to
   `max_activation_time_ms` (accounts for current hop states — warm hops
   contribute near-zero).
3. **Bandwidth**: Every link MUST have `available_bps >= required_bps` for the
   negotiated format.
4. **Device capacity**: Every device touched by the pipeline MUST have sufficient
   resources to support the pipeline's cost, including peak activation cost.
   If any resource on any device would exceed capacity, the pipeline MUST be
   rejected.
5. **Resource budget**: Every device with a resource budget MUST NOT exceed its
   hard limits after adding this pipeline's cost. Devices in `adaptive` mode
   MAY pass this check with a degraded pipeline configuration.
6. **Power budget**: Every voltage rail on every device in the pipeline MUST have
   sufficient current headroom for the pipeline's power cost. RGB pipelines
   MUST be checked against LED power calculations.
7. **Loss**: Every link MUST have `loss_rate <= max_loss`.
8. **Jitter**: Every link MUST have `jitter_p99 <= max_jitter_ms`.
9. **Format**: At least one format in the negotiated intersection MUST NOT be
   in `forbidden_formats` and (if specified) MUST be in `required_formats`.
10. **Hops**: Pipeline length MUST be less than or equal to `max_hops`.
11. **Conversions**: Number of format changes MUST be less than or equal to
    `max_conversions`.
12. **Encryption**: If `required`, every link MUST support encryption.

If no pipeline satisfies all constraints, the degradation policy MUST be applied.

### 3. Path Computation

The router MUST use a modified Dijkstra's algorithm over the graph:

1. Build the graph from discovered devices, ports, and links.
2. For each candidate source-to-destination pair:
   a. Enumerate all paths (bounded by `max_hops` or a RECOMMENDED reasonable default).
   b. For each path, compute format negotiation.
   c. If format negotiation fails (empty intersection), the router SHOULD try
      inserting converters.
   d. Apply constraint filter.
   e. Compute cost for surviving paths.
3. Select the lowest-cost path.
4. Fixate formats on each link.
5. Return the Pipeline.

**Converter insertion**: When two adjacent ports have incompatible formats, the
router SHOULD search for a converter plugin that bridges them. Each converter is
modelled as a virtual device with a sink port (input format) and source port
(output format). Inserting a converter adds a hop with its own latency cost.
The router MUST try all available converters and pick the one with lowest cost.

### 4. Re-evaluation Triggers

Pipelines MUST be re-evaluated when the graph changes. Changes that trigger
re-evaluation:

| Trigger | Source | Response |
|---------|--------|----------|
| Device added/removed | Hotplug event from device plugin | Full re-evaluation of affected pipelines |
| Link metrics changed | Periodic measurement, passive monitoring | Re-evaluate if metrics cross constraint boundaries |
| Link failed | Transport plugin reports failure | Immediate failover to next-best pipeline |
| Intent changed | User action, scenario switch, automation | Full re-evaluation with new intent |
| Bandwidth contention | Measured available bandwidth dropped | Re-evaluate affected pipelines, MAY degrade |
| Device resource pressure | CPU/memory/GPU usage approaching limits | Degrade or cool warm pipelines on pressured device |
| Budget exceeded | Agent reports usage above backoff threshold | Reduce pipeline quality on that device |
| Power rail pressure | Voltage drop or current approaching rail capacity | Scale RGB brightness, cool warm pipelines, alert |
| External event | Meeting started (meeting_detect.py), Zoom call detected | Proactive re-evaluation with updated bandwidth expectations |

**Reactive** triggers (link failure, metric threshold crossing) MUST cause
immediate re-evaluation. **Proactive** triggers (periodic measurement, external
events) SHOULD cause background re-evaluation — the new pipeline is computed but
SHOULD NOT be activated until the current one actually degrades or the proactive
assessment shows a better path.

**Predictive** triggers (meeting detection, application launch) SHOULD pre-compute
alternative pipelines. Example: a Zoom call is about to start — the router
pre-computes a pipeline that uses less bandwidth for the KVM session, ready to
swap instantly when contention is detected.

### 5. Switching and Activation

Pipeline switching has two distinct time components:

1. **Route computation** — deciding which pipeline to use. This MUST be
   pre-computed at graph-change time, not at switch time. Cost: <1ms.

2. **Pipeline activation** — bringing the selected pipeline's hops from their
   current state to `active`. This is where real time is spent, and it varies
   enormously by component:

| Pipeline state at switch time | Activation time | How |
|-------------------------------|-----------------|-----|
| Warm (all hops initialised) | <10ms | Just start data flow |
| Partially warm (some hops cold) | Slowest cold hop | Warm cold hops in parallel where possible |
| Fully cold (nothing running) | Sum of critical path | Sequential where dependencies exist |

The router MUST NOT perform format negotiation, path computation, or measurement
at switch time. All of that MUST have happened earlier. Switching is activating a
pre-computed pipeline — the cost is purely the activation time of its hops.

**Warm pipeline switching** is the fast path. When the router keeps pipelines
warm for likely scenario switches, activation is near-instantaneous:
HID redirect (<5ms) + video/audio already flowing into discard (<1ms to
redirect to real output).

**Cold pipeline switching** MUST be honest about its cost. If switching requires
an HDMI matrix change (2s for HDCP) and ffmpeg startup (3s), the router MUST
report a 3-5s activation time. The user sees this in the UI and MAY choose to
keep that pipeline warm, accept the delay, or restructure the path.

**Activation time as a routing input**: The intent's `max_activation_time_ms`
constraint MUST filter candidate pipelines by activation time. A `gaming` intent
might set `max_activation_time_ms: 500`, which forces the router to either
select a pipeline that is already warm or reject paths with slow components
(effectively requiring pre-warming for the gaming scenario).

The cost model MAY optionally include activation time as a factor:

```
cost(link) += w_activation * activation_time_ms  # if intent cares about fast switching
```

This penalises paths through slow-switching devices, making the router prefer
direct paths over paths through external switches when activation time matters.

### 6. Remediation

When a link or device fails, the router's first response MUST be to failover to
an alternative pipeline. But the router MAY also attempt to **fix** the broken
path so it becomes available again. Remediation is the model for expressing what
corrective actions are possible, how safe they are, and when to attempt them.

**Remediation capabilities on devices and links**:

Every device and link SHOULD advertise what remediation actions it supports:

```yaml
RemediationCapability:
  action: string                # action identifier
  target: string                # "device", "link", "port", "service"
  safety: RemediationSafety     # how risky is this action
  disruption: DisruptionLevel   # what gets disrupted
  estimated_duration_ms: uint   # how long the action takes
  success_rate: float?          # historical success rate (0.0-1.0, if known)
  cooldown_ms: uint?            # minimum time between attempts
  max_attempts: uint?           # maximum retry count before escalating
  prerequisites: string[]?      # conditions that must be true (e.g., "pipeline_not_active")

RemediationSafety: enum
  safe                          # no risk — can always attempt (process restart, cache clear)
  disruptive                    # disrupts this device's active pipelines (USB rebind, service restart)
  destructive                   # affects other devices or loses state (hub power cycle, node reboot)
  manual                        # requires human action (replace cable, reseat card)

DisruptionLevel: enum
  none                          # transparent to user
  momentary                     # <1s interruption
  brief                         # 1-10s interruption
  extended                      # >10s interruption, user will notice
  full                          # device goes offline entirely
```

**Standard remediation actions**:

| Action | Target | Safety | Disruption | Description |
|--------|--------|--------|-----------|-------------|
| `process_restart` | service | safe | momentary | Restart a software service (ffmpeg, audio bridge) |
| `usb_rebind` | device | disruptive | brief | Unbind/rebind USB device via sysfs |
| `usb_hub_power_cycle` | device | destructive | extended | Power cycle a USB hub (affects all devices on hub) |
| `v4l2_reopen` | port | safe | momentary | Close and reopen V4L2 device |
| `pipewire_reconnect` | link | safe | momentary | Destroy and recreate PipeWire link |
| `network_interface_restart` | link | disruptive | brief | ifdown/ifup on network interface |
| `wifi_channel_scan` | link | disruptive | brief | Trigger WiFi channel re-evaluation |
| `node_reboot` | device | destructive | full | Reboot the entire node |
| `target_power_cycle` | device | destructive | full | LoM power cycle of target machine |
| `cable_reseat` | link | manual | full | User physically reseats a cable |
| `device_replace` | device | manual | full | User replaces faulty hardware |

**Remediation policy**:

Remediation actions MUST respect safety levels.

```yaml
RemediationPolicy:
  mode: string                  # "auto", "confirm", "notify", "disabled"
  auto_actions: RemediationSafety[]  # which safety levels to auto-attempt
                                     # (default: ["safe"] — only safe actions are automatic)
  confirm_actions: RemediationSafety[]  # which to attempt after user confirmation
  notify_actions: RemediationSafety[]   # which to only notify about
  escalation_chain: EscalationStep[]    # what to do when remediation fails

EscalationStep:
  after_attempts: uint          # escalate after this many failed attempts
  action: string                # next action to try, or "alert"
  delay_ms: uint                # wait this long before escalating
```

Default policy: `safe` remediation MAY be automatic. `disruptive` actions
SHOULD require confirmation. `destructive` actions MUST NOT be automatic without
explicit policy configuration — they are notify-only by default. `manual` actions
MUST generate a recommendation to the user. This matches the agent approval
model — the same safety philosophy applied to infrastructure.

### 7. Intent Bindings

Intents define what the user wants. Triggers detect what is happening. Intent
bindings connect them — when a condition is observed, automatically apply an
intent to affected pipelines.

Intent bindings MUST be evaluated on every matching event.

```yaml
IntentBinding:
  id: string
  name: string                  # human-readable ("Gaming mode on game launch")
  conditions: BindingCondition[]
  condition_mode: string        # "all" (AND) or "any" (OR)
  intent: string                # intent name to apply when conditions match
  scope: BindingScope           # which pipelines are affected
  revert: RevertPolicy          # what happens when conditions stop matching
  priority: uint                # when multiple bindings match, highest priority wins
  enabled: bool

BindingCondition:
  source: string                # what to observe
  field: string                 # which field on the source
  op: string                    # "eq", "neq", "gt", "lt", "in", "contains", "matches"
  value: any                    # comparison value

BindingScope:
  target: string                # "all", "node", "device", "pipeline"
  target_id: string?            # specific node/device/pipeline (null = all matching)
  streams: string[]?            # which media types to affect (null = all)

RevertPolicy:
  mode: string                  # "revert" (restore previous intent),
                                # "hold" (keep new intent until manually changed),
                                # "timeout" (revert after duration)
  timeout_ms: uint?             # for "timeout" mode
```

**Condition sources** — what MAY trigger an intent binding:

| Source | Fields | Example |
|--------|--------|---------|
| `activity` | `state`, `title`, `app_id`, `platform` | `activity.state eq gaming` |
| `device` | `type`, `id`, `health`, any property | `device.type eq gamepad` (gamepad connected) |
| `sensor` | `reading`, `value`, `type` | `sensor.type eq motion AND sensor.value eq true` |
| `time` | `hour`, `day`, `weekday` | `time.hour gt 22` (after 10pm) |
| `power` | `battery_percent`, `charging`, `source` | `power.battery_percent lt 20` |
| `link` | `latency_ms`, `jitter_ms`, `loss` | `link.latency_ms gt 100` (high-latency path) |
| `presence` | `user_zone`, `idle_seconds`, `active_input` | `presence.idle_seconds gt 300` |
| `calendar` | `event_active`, `event_title`, `attendees` | `calendar.event_active eq true` |
| `input` | `active_keyboard`, `active_mouse` | `input.active_keyboard eq wireless-kb-1` |

**Examples**:

```yaml
# Game detected -> switch to gaming intent
- id: auto-gaming
  conditions:
    - { source: activity, field: state, op: eq, value: gaming }
  intent: gaming
  scope: { target: node, target_id: gaming-pc }
  revert: { mode: revert }

# User idle for 5 minutes -> switch to observe (save resources)
- id: idle-observe
  conditions:
    - { source: presence, field: idle_seconds, op: gt, value: 300 }
  intent: observe
  scope: { target: all }
  revert: { mode: revert }
  priority: 10

# Wireless keyboard active -> user is at couch, prefer TV output
- id: couch-mode
  conditions:
    - { source: input, field: active_keyboard, op: eq, value: wireless-kb-couch }
  intent: desktop
  scope: { target: all }
  revert: { mode: revert }

# Battery low on phone -> reduce phone screen mirror quality
- id: phone-battery-saver
  conditions:
    - { source: power, field: battery_percent, op: lt, value: 20 }
    - { source: device, field: type, op: eq, value: phone }
  intent: preview
  scope: { target: device, streams: ["video"] }
  revert: { mode: revert }
```
