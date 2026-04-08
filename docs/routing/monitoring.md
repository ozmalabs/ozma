# Observability and Monitoring

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

The routing graph is a monitoring platform by construction. The data the router
needs to make routing decisions — device resources, link health, power state,
versions, topology — is exactly the data a monitoring platform needs. Any system
that joins the Ozma mesh MUST be automatically observable without a separate
monitoring agent, secondary data collection pipeline, or additional configuration.

## Specification

### Monitoring platform by construction

The routing graph is a **monitoring platform by construction**. The data the
router needs to make routing decisions — device resources, link health, power
state, versions, topology — is exactly the data a monitoring platform needs.
Exposing it is a read path on data that already exists, not a separate system.

This is a deliberate architectural property: any system that joins the Ozma
mesh is automatically monitored. There is no separate monitoring agent to
install, no secondary data collection pipeline, no configuration. The node
needs to report its CPU usage so the router can avoid overloading it — that
same data stream is your CPU monitoring. The power model needs to track
voltage rails so the router can avoid brownouts — that same data is your
power monitoring. The device database needs to know firmware versions so the
router can check protocol compatibility — that same data is your asset
inventory.

The routing graph MUST be fully observable via the control plane API. All
state changes SHOULD be recorded in the journal (if journal is enabled).

**What you get for free by being in the mesh**:

| Monitoring domain | Source in the routing spec | Traditional tool replaced |
|------------------|--------------------------|---------------------------|
| Resource utilisation (CPU, memory, GPU, disk) | Device Capacity | Prometheus node_exporter, Datadog agent |
| Network health (latency, jitter, loss, bandwidth) | Link properties | SmokePing, Nagios, PRTG |
| Power (voltage, current, rail health, battery) | Power Model | Custom INA219 scripts, UPS monitoring |
| Thermal | ResourceType | lm-sensors polling |
| Asset inventory (hardware, versions, topology) | Device Database + Versioning | Snipe-IT, GLPI, Lansweeper |
| Topology mapping | Topology Discovery | nmap, network mapping tools |
| Service health | Managed Services | Uptime Kuma, Healthchecks.io |
| USB device tree | Layer 1 Hardware Enumeration | lsusb scripts |
| Storage health | StorageSpec | smartctl polling |
| Event stream (state changes, alerts, threshold crossings) | Events | Alertmanager, PagerDuty integration |

### Graph queries

```
GET /api/v1/routing/graph          # full graph (devices, ports, links)
GET /api/v1/routing/graph/devices  # all devices with ports
GET /api/v1/routing/graph/links    # all links with metrics
GET /api/v1/routing/pipelines      # all active and standby pipelines
GET /api/v1/routing/pipelines/{id} # pipeline detail with per-hop metrics
GET /api/v1/routing/intents        # all defined intents (built-in + custom)
GET /api/v1/routing/intents/{name} # intent definition
GET /api/v1/routing/devices/{id}/capacity  # device resource capacity and current usage
GET /api/v1/routing/devices/{id}/budget    # device resource budget and compliance
GET /api/v1/routing/pressure               # all devices with resource pressure summary
PUT /api/v1/routing/devices/{id}/budget    # set/update resource budget for a device
```

### Monitoring queries

Queries designed for monitoring dashboards and alerting, not just routing
diagnostics:

```
# Time-series data for any device metric
GET /api/v1/monitoring/metrics/{device_id}?keys=cpu_percent,memory_mb&range=1h
# Returns: timestamped metric values over the requested window.

# Fleet-wide health summary
GET /api/v1/monitoring/health
# Returns: per-device health status (healthy, degraded, critical, unreachable),
# aggregated by device type, with worst-offender highlighting.

# All current alerts/warnings across the fleet
GET /api/v1/monitoring/alerts
# Returns: active threshold crossings (power, thermal, resource, network).

# Historical link quality between two devices
GET /api/v1/monitoring/link/{link_id}/history?range=24h
# Returns: latency/jitter/loss/bandwidth time series.

# Power dashboard — all rails across all devices
GET /api/v1/monitoring/power
# Returns: per-rail voltage, current, capacity, health status.

# Asset inventory snapshot
GET /api/v1/monitoring/inventory
# Returns: all devices with hardware info, versions, location, health.
# Includes: serial numbers, MAC addresses, firmware versions, purchase/first-seen
# date, physical location, current health, connected topology.
# Exportable as CSV/JSON for compliance evidence, insurance, auditing.

# Asset lifecycle
GET /api/v1/monitoring/inventory/{device_id}/history
# Returns: complete lifecycle for one device — first seen, every firmware update,
# every location change, every state change, health trend, connected-to history.

# Asset search
GET /api/v1/monitoring/inventory/search?serial=ABC123
GET /api/v1/monitoring/inventory/search?mac=AA:BB:CC:DD:EE:FF
GET /api/v1/monitoring/inventory/search?location=rack-1
GET /api/v1/monitoring/inventory/search?status=degraded
# Search across all asset fields — serial, MAC, location, type, status, vendor.

# Sankey / flow diagram data for any resource type
GET /api/v1/monitoring/sankey?type=bandwidth
GET /api/v1/monitoring/sankey?type=power
GET /api/v1/monitoring/sankey?type=audio
GET /api/v1/monitoring/sankey?type=storage_io
GET /api/v1/monitoring/sankey?device_id={id}&type=bandwidth
# Returns: directed graph of flows with source, destination, and magnitude
# at each hop. Renderable as a Sankey diagram, flow map, or treemap.
# type=bandwidth: bytes/sec through every link, showing shared segments and bottlenecks
# type=power: watts from PSU rails through every device to every consumer
# type=audio: audio signal paths with gain, format, and latency per hop
# type=storage_io: I/O flow from applications through controllers to drives
# Exportable as CSV/JSON for compliance evidence.
```

### Diagnostic queries

```
# "Why did the router choose this pipeline?"
GET /api/v1/routing/explain?source={port}&dest={port}&intent={name}
# Returns: all candidate pipelines, their costs, which constraints eliminated
# each rejected candidate, and why the selected pipeline won.

# "What would happen if this link failed?"
GET /api/v1/routing/simulate?fail_link={link_id}
# Returns: which pipelines would be affected, what alternatives exist,
# expected degradation.

# "Can this intent be satisfied?"
POST /api/v1/routing/evaluate
# Body: { source, destination, intent }
# Returns: best pipeline if possible, or the closest match with which
# constraints are violated and what degradation would be needed.

# "What's consuming power on this device?"
GET /api/v1/routing/explain/power/{device_id}
# Returns: per-rail breakdown, per-function power attribution,
# headroom analysis, warnings.
```

### Metric retention and export

The routing graph is real-time — it reflects current state. For historical
data, metrics are retained locally with configurable windows. Metric retention
MUST support at least the configured retention window.

```yaml
MetricRetention:
  high_resolution: duration     # 1-second samples (default: 1 hour)
  medium_resolution: duration   # 1-minute aggregates (default: 24 hours)
  low_resolution: duration      # 15-minute aggregates (default: 30 days)
  export_targets: MetricsSink[] # external systems to push to
```

Export to external systems uses the `metrics_sink` device type. Supported
formats: Prometheus exposition (scrape endpoint), OTLP push, StatsD, syslog.
The controller acts as the collection point — nodes report to the controller,
the controller exports to external sinks.

For users who don't want external monitoring infrastructure, the built-in
retention is sufficient. For users who already have Prometheus/Grafana or
Datadog, the export path feeds into their existing stack — Ozma data appears
alongside everything else they monitor.

### Events

State changes emit events on the WebSocket event stream. These serve both
routing (the router reacts to them) and monitoring (dashboards and alerting
consume them). Events MUST include a timestamp, severity, and structured
payload.

```
# Pipeline lifecycle
routing.pipeline.created    # new pipeline computed
routing.pipeline.warming    # pipeline transitioning from standby to warm
routing.pipeline.warm       # pipeline is warm and ready for fast activation
routing.pipeline.activating # pipeline transitioning from warm/standby to active
routing.pipeline.activated  # pipeline is active — data flowing
routing.pipeline.cooling    # pipeline transitioning from warm to standby (resource release)
routing.pipeline.degraded   # pipeline quality dropped below intent preferences
routing.pipeline.failed     # pipeline link failed, failover in progress
routing.pipeline.recovered  # failed pipeline restored or replaced

# Device lifecycle
routing.device.discovered   # new device added to graph
routing.device.removed      # device removed from graph
routing.device.pressure     # device resource pressure changed (approaching/exceeding limits)
routing.device.budget_breach # device exceeded resource budget backoff threshold

# Link and network
routing.link.measured       # link metrics updated
routing.link.degraded       # link quality dropped below threshold
routing.link.recovered      # link quality restored

# Power
routing.power.rail_warning       # voltage or current approaching limits
routing.power.rail_critical      # voltage below min or current exceeding capacity
routing.power.budget_exceeded    # device power draw exceeds source capacity
routing.power.pd_negotiated      # USB PD negotiation completed
routing.power.battery_low        # battery below threshold
routing.power.rgb_power_limited  # RGB brightness scaled down due to power limit

# Versioning
device.version.update_available  # new version detected
device.version.updating          # applying update
device.version.updated           # update succeeded
device.version.update_failed     # update failed
device.version.incompatible      # version mismatch

# Intent and routing
routing.intent.changed      # intent definition updated

# Thermal
device.thermal.warning       # approaching thermal throttle
device.thermal.throttling    # actively thermal throttling
device.thermal.recovered     # temperature returned to normal

# Trend / predictive
trend.degradation_detected   # metric trending toward failure (e.g., increasing USB error rate)
trend.capacity_warning        # resource approaching exhaustion at current rate (storage, battery wear)
trend.lifetime_estimate       # estimated time until failure/exhaustion, based on trend
trend.anomaly_detected        # metric deviated significantly from historical baseline
```

Every event includes a timestamp, the device/link/pipeline ID, severity
(`info`, `warning`, `critical`), and a structured payload. Events can be
forwarded to notification sinks based on configurable rules —
"send Slack message on any `critical` event", "email on `device.version.update_failed`".

### State change journal

Every state change in the routing graph — device discovered/removed, link
up/down, port connected/disconnected, power rail change, version change,
configuration change — is optionally recorded to a persistent journal. This
is distinct from the real-time event stream: events are ephemeral (consumed
by listeners), the journal is durable (queryable history).

**What constitutes a state change**:

Any mutation to the routing graph is a state change. The journal captures
the graph diff — what changed, from what to what, when, and why.

Journal entries MUST include timestamp, entity, before/after state, and trigger.

```yaml
StateChangeRecord:
  id: uint64                    # monotonically increasing sequence number
  timestamp: timestamp          # when the change occurred
  change_type: StateChangeType  # what kind of change
  entity_type: string           # "device", "port", "link", "pipeline", "rail", "config"
  entity_id: string             # which entity changed
  device_id: string?            # device the change relates to (if applicable)
  before: any?                  # previous state (null for creation)
  after: any?                   # new state (null for deletion)
  trigger: string?              # what caused the change ("hotplug", "user", "router",
                                # "measurement", "timeout", "enrollment", "ota")
  metadata: PropertyBag?        # additional context

StateChangeType: enum
  # Device lifecycle
  device_added                  # new device appeared in graph (USB hotplug, mDNS, enrollment)
  device_removed                # device left the graph (unplug, timeout, revocation)
  device_state_changed          # device property changed (health, capacity, version)

  # Port lifecycle
  port_connected                # something plugged into this port
  port_disconnected             # something unplugged from this port
  port_state_changed            # port properties changed (format, bandwidth, active state)

  # Link lifecycle
  link_created                  # new link between ports
  link_removed                  # link broken
  link_state_changed            # link metrics changed significantly (latency, jitter, loss)

  # Pipeline lifecycle
  pipeline_created
  pipeline_activated
  pipeline_warmed
  pipeline_cooled
  pipeline_degraded
  pipeline_failed
  pipeline_recovered
  pipeline_destroyed

  # Power
  rail_voltage_changed          # voltage moved outside normal band
  rail_current_changed          # significant current change
  power_source_changed          # device switched power source (e.g., battery <-> mains)
  pd_negotiation_changed        # USB PD renegotiated

  # Configuration
  config_changed                # user changed a setting
  intent_changed                # intent definition updated
  budget_changed                # resource budget modified
  gadget_function_changed       # USB gadget function added/removed/modified

  # Identity
  device_enrolled               # device joined mesh
  device_revoked                # device removed from mesh
  certificate_issued            # new certificate from mesh CA
  certificate_expired           # certificate expired
```

**Storage policy**:

```yaml
JournalPolicy:
  enabled: bool                 # default: true
  storage: string               # "sqlite", "append_log", "memory_ring"
  retention: JournalRetention
  filters: JournalFilter[]      # which changes to record (default: all)

JournalRetention:
  max_records: uint?            # maximum records to keep (oldest evicted)
  max_age_days: uint?           # delete records older than this
  max_size_mb: uint?            # maximum storage size

JournalFilter:
  include: StateChangeType[]?   # only record these types (null = all)
  exclude: StateChangeType[]?   # never record these types
  min_severity: string?         # only record changes at or above this severity
  devices: string[]?            # only record changes for these devices (null = all)
```

Default policy: record everything, keep 30 days, cap at 100 MB. For
resource-constrained nodes, the policy can be narrowed — e.g., only record
device adds/removes and power events, keep 7 days.

**Querying the journal**:

```
GET /api/v1/monitoring/journal?range=24h
GET /api/v1/monitoring/journal?device_id={id}&range=7d
GET /api/v1/monitoring/journal?change_type=device_added,device_removed&range=30d
GET /api/v1/monitoring/journal?entity_type=link&range=1h
GET /api/v1/monitoring/journal/{sequence_id}  # single record with full before/after
```

**Use cases**:

- **"When was this USB device last plugged in?"** — query `port_connected`
  for the device's USB port
- **"What changed in the last hour?"** — query all changes, range=1h
- **"Why did the RGB strip go dark at 3am?"** — query power events and
  device state changes for that time window
- **"Show me every firmware update across the fleet this month"** — query
  `device_state_changed` where the version field differs
- **Compliance audit trail** — the journal provides a tamper-evident record
  of every graph mutation (ties into the existing hashchained audit log)
- **Debugging intermittent issues** — USB device dropping and reconnecting
  shows up as repeated `port_disconnected` / `port_connected` pairs with
  timestamps, revealing the pattern
- **Asset lifecycle tracking** — every device has a complete history from
  first discovery to current state: when it was first seen (acquisition/
  deployment), every firmware update, every physical location change, every
  health state transition, every connection change. This is IT asset
  management (ITAM) built into the routing graph — no separate CMDB or
  spreadsheet. The journal + hardware identity + device database entry
  together give you: what it is, which specific one, where it is, what
  state it's in, and everything that ever happened to it.
- **Internal hardware tracking** — for Ozma Labs and any organisation: track
  provenance and status of every piece of hardware from acquisition through
  deployment to disposal. Know which dev board ran which firmware, which
  capture card has had USB errors, which SFP module was reflashed, which
  PSU cable belongs to which PSU. The state change journal is the source
  of truth — queryable, exportable, auditable.

**Relationship to the audit log** (`controller/audit_log.py`): The audit log
is a hashchained compliance record of security-relevant actions (authentication,
authorization, configuration changes). The state change journal is broader —
it records all graph mutations including non-security events (USB hotplug,
link metric changes, power fluctuations). The audit log is a strict subset
of the journal with cryptographic integrity guarantees. Both can be enabled
independently.

### Trend analysis

The journal and metric retention store historical data. Trend analysis detects
patterns in that data — degradation over time, capacity approaching exhaustion,
anomalous behaviour — and emits predictive events before problems occur.
Trend alerts SHOULD be emitted when degradation or capacity exhaustion is
detected.

The spec defines the **output model** (what trend alerts look like), not the
algorithms (that's implementation).

**Trend alert structure**:

```yaml
TrendAlert:
  id: string
  type: TrendAlertType
  device_id: string             # affected device
  metric: string                # which metric is trending ("usb_error_rate",
                                # "link_jitter_p95", "storage_used_percent",
                                # "battery_health_percent", "rail_voltage_5v")
  current_value: float          # current value of the metric
  trend_direction: string       # "increasing", "decreasing", "unstable"
  rate_of_change: float         # change per hour (in metric's native unit)
  projected_threshold_value: float?  # when will it cross the threshold?
  projected_time: timestamp?    # estimated time of threshold crossing
  confidence: float             # 0.0-1.0 confidence in the projection
  window: string                # time window the trend was computed over ("24h", "7d", "30d")
  severity: string              # "info", "warning", "critical"
  recommendation: string?       # human-readable suggestion ("Replace USB cable",
                                # "Upgrade power supply", "Expand storage")

TrendAlertType: enum
  degradation                   # metric trending toward failure
  capacity_exhaustion           # resource approaching full (storage, battery, power)
  lifetime_estimate             # projected device/component lifetime
  anomaly                       # metric deviated from historical baseline
  recurring_failure             # same failure happening repeatedly (e.g., USB disconnect/reconnect)
```

**What can be trended**:

| Metric | Trend indicates | Example alert |
|--------|----------------|---------------|
| USB error rate | Cable/connector degradation | "USB error rate on Node 1 port 2 increasing 3%/week — cable may be failing" |
| Link jitter (p95) | Network path degradation | "WiFi jitter to Node 3 increased 40% over 7 days — possible interference" |
| Storage used % | Capacity exhaustion | "Recording storage on Node 2 will be full in ~12 days at current rate" |
| Battery health % | Battery wear | "Phone battery at 78% health, losing ~2%/month" |
| Rail voltage | Power supply degradation | "5V rail on Node 1 averaging 4.81V, down from 4.92V 30 days ago" |
| SSD write latency | Drive wear | "NVMe write latency up 25% over 90 days — approaching endurance limit" |
| Thermal (idle temp) | Cooling degradation | "Idle temp on Node 4 trending up 0.5C/week — check airflow/thermal paste" |
| Reconnection frequency | Intermittent hardware fault | "Capture card on Node 1 has disconnected 4 times this week, up from 0 last month" |

**API**:

```
GET /api/v1/monitoring/trends                         # all active trend alerts
GET /api/v1/monitoring/trends/{device_id}             # trends for a specific device
GET /api/v1/monitoring/trends/{device_id}/{metric}    # trend detail with historical data points
```
