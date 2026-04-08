# Information Quality and Data Freshness

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

Every measured or reported property in the routing graph carries a provenance
tag indicating how much the router should trust it. This specification defines
the trust hierarchy, qualified value metadata, data freshness tracking, refresh
scheduling, quality decay, and the rules by which routing decisions incorporate
information quality.

## Specification

### Quality Levels

Every measured property MUST carry an `InfoQuality` provenance tag.

```yaml
InfoQuality: enum
  user        # explicitly set by the user — highest trust
  measured    # from active probing or passive measurement — high trust
  inferred    # derived from measured data + known/assumed parameters — high-medium trust
  reported    # from OS/driver API (lsusb, boltctl, WMI, PipeWire) — medium trust
  commanded   # we sent a command but cannot confirm it was applied — medium-low trust
  spec        # from device specification or standard (USB 3.0 = 5 Gbps) — low trust
  assumed     # heuristic or default — lowest trust, override as soon as possible
```

Trust ordering MUST be: `user > measured > inferred > reported > commanded > spec > assumed`.

Higher-trust values MUST override lower-trust values for the same property.
When `measured` contradicts `spec` (e.g. a USB 3.0 port only achieving USB 2.0
speeds behind a hub), `measured` wins.

**`commanded` quality**: This level exists specifically for write-only devices.
When the router sends a command to a device (e.g., "switch to input 3") but
receives no confirmation, the resulting state is `commanded`. The router MUST
treat `commanded` as better than a blind assumption (`assumed`) because the
command was actively issued, but worse than `reported` because the outcome
cannot be verified. If a pipeline built on `commanded` state fails to deliver
data, the router knows the switch state is the likely culprit and MAY retry,
try an alternative path, or escalate to the user.

`commanded` values MUST NOT decay in the same way as `measured` values -- they
remain `commanded` indefinitely until either confirmed by measurement (e.g.
video starts flowing, upgrading to `measured`) or contradicted by observation
(e.g. no video after timeout, downgrading to `assumed`).

### Quality Metadata and Data Freshness

Every property that carries a quality tag MUST also carry metadata about the
measurement:

```yaml
QualifiedValue<T>:
  value: T                      # the actual value
  quality: InfoQuality          # provenance
  source: string                # where this came from ("lsusb", "iperf", "user override")
  measured_at: timestamp?       # when this was measured (null for spec/assumed)
  confidence: float?            # 0.0-1.0, statistical confidence (for measured values)
  sample_count: uint?           # number of measurements (for measured values)
  refresh_class: string?        # which refresh schedule this value follows (see below)
```

**Data freshness is a first-class concern.** Different data has different
natural refresh cadences, different collection costs, and different staleness
tolerances. The system MUST be explicit about all three -- consumers need to
know not just *when* data was collected, but *why* it is that old and *when*
it will next be refreshed.

**RefreshSchedule** -- global primitive defining how often each class of
data is collected, and what it costs to collect it:

```yaml
RefreshSchedule:
  classes: RefreshClass[]

RefreshClass:
  id: string                    # class identifier (referenced by QualifiedValue.refresh_class)
  name: string                  # human-readable
  default_interval_s: float     # how often to refresh by default
  min_interval_s: float?        # fastest allowed refresh (protects against overload)
  adaptive: bool                # does the interval adjust based on conditions?
  cost: RefreshCost             # what it costs to collect this data
  staleness: StalenessPolicy    # how to handle aged-out data
  trigger_refresh_on: string[]? # events that trigger an immediate refresh outside the schedule

RefreshCost:
  cpu_impact: string            # "negligible", "low", "moderate", "high"
  io_impact: string             # "none", "disk_read", "disk_write", "network"
  device_impact: string         # "none", "wake_disk", "interrupt_device", "bus_contention"
  duration_ms: float?           # typical collection time
  notes: string?                # why this cost exists

StalenessPolicy:
  fresh_threshold_s: float      # below this age: data is fresh, full trust
  stale_threshold_s: float      # above this age: data is stale, reduced trust
  expired_threshold_s: float?   # above this age: data is expired, flag prominently
  action_on_stale: string       # "degrade_quality" (reduce InfoQuality),
                                # "flag_only" (mark stale but keep quality),
                                # "trigger_refresh" (attempt immediate refresh)
  action_on_expired: string     # "degrade_quality", "flag_only", "remove_from_graph"
```

### Standard Refresh Classes

| Class | Default interval | Cost | Staleness | Rationale |
|-------|-----------------|------|-----------|-----------|
| `realtime_metrics` | 1-5s | Negligible (read /proc, sysfs) | Stale at 15s, expired at 60s | CPU %, memory, temperature, fan RPM -- cheap to read, changes constantly |
| `network_health` | 5-10s | Low (ping, packet counters) | Stale at 30s, expired at 120s | Latency, jitter, loss -- needs periodic probing |
| `link_bandwidth` | 30-60s | Moderate (passive measurement) | Stale at 5min, expired at 30min | Available bandwidth -- measured from traffic flow |
| `usb_topology` | On event + 5min poll | Moderate (lsusb -t, udevadm) | Stale at 10min, expired at 1h | USB tree -- changes on hotplug, otherwise static |
| `pcie_topology` | On boot + 1h poll | Low (lspci, sysfs) | Stale at 2h, expired at 24h | PCIe devices -- rarely change at runtime |
| `smart_health` | 1-24h | Moderate (disk I/O, can wake spun-down disks) | Stale at 48h, expired at 7d | Drive health -- querying wakes sleeping disks |
| `sfp_dom` | 30s | Low (I2C read from module) | Stale at 2min, expired at 10min | Optical power, temperature -- cheap, changes with conditions |
| `firmware_versions` | 24h + on boot | Low (fwupd query, dmidecode) | Stale at 48h, expired at 7d | Changes only on update -- no reason to poll frequently |
| `power_rails` | 1-5s (if INA219) | Negligible (I2C read) | Stale at 15s, expired at 60s | Voltage/current -- changes with load |
| `power_rails` | 60s (if inferred) | Low (read voltage, compute) | Stale at 5min, expired at 30min | Inferred current -- less time-sensitive |
| `pdu_metering` | 10-60s | Low (SNMP poll) | Stale at 2min, expired at 10min | Per-outlet power -- via SNMP/vendor API |
| `ups_state` | 10-30s | Low (NUT query) | Stale at 60s, expired at 5min | Battery state -- critical during outage |
| `bios_version` | On boot only | Negligible (dmidecode) | Never stale (does not change at runtime) | Static until reboot after flash |
| `device_db_match` | On first discovery | Negligible (local lookup) | Never stale (cached) | Device identification -- does not change |
| `bluetooth_connection` | 5-10s | Low (bluetoothctl, D-Bus) | Stale at 30s, expired at 2min | RSSI, codec, battery -- changes with distance |
| `wifi_signal` | 5-10s | Low (iw, nl80211) | Stale at 30s, expired at 2min | RSSI, channel utilisation -- changes constantly |
| `thermal_zone` | 1-5s | Negligible (sysfs) | Stale at 15s, expired at 60s | Zone temperatures -- critical for fan control |
| `room_occupancy` | 10-60s | Varies (camera inference: high; PIR: negligible) | Stale at 5min, expired at 30min | Presence detection -- impacts intent bindings |

### Adaptive Refresh

RefreshSchedule classes SHOULD use adaptive intervals when `adaptive: true`.
The refresh interval adjusts based on conditions:

- **Under pressure**: If a device is near a thermal/power/resource limit,
  its metrics SHOULD refresh faster (5s to 1s) for tighter control.
- **Idle**: If nothing is changing, refresh SHOULD slow down (1s to 10s) to
  reduce CPU overhead and I/O.
- **Active pipeline**: Devices in an active pipeline SHOULD refresh their
  relevant metrics faster than idle devices.
- **On battery**: An agent on a laptop SHOULD reduce refresh rates to save
  power.
- **Constrained device**: A node with limited CPU (Pi Zero) SHOULD use longer
  intervals than a full controller.

Implementations MUST NOT refresh more frequently than `min_interval_s`.

The refresh schedule is configurable per device, per class. A user who cares
about thermal monitoring MAY set `thermal_zone` to 1s refresh on their
overclocked server. A user on a Pi Zero MAY set `usb_topology` to 30-minute
polling. The defaults are sensible for typical hardware.

### Device-Level Freshness

Every device in the graph MUST track `DeviceFreshness`:

```yaml
DeviceFreshness:
  online: bool                  # is this device currently reachable?
  last_contact: timestamp       # when we last heard from this device
  last_full_refresh: timestamp  # when all data classes were last refreshed
  per_class: ClassFreshness[]   # per-refresh-class status

ClassFreshness:
  class: string                 # refresh class ID
  last_refreshed: timestamp     # when this class was last refreshed
  next_refresh: timestamp?      # when the next refresh is scheduled
  age_s: float                  # current age in seconds
  state: string                 # "fresh", "stale", "expired", "never_collected"
  error: string?                # if last refresh failed: why
```

`DeviceFreshness` sits on the Device primitive alongside `capacity`,
`power_profile`, and `version`. API consumers can check: "Is this device's
thermal data fresh? When will it next be updated? Is the SMART data expired
because the drive is sleeping and we do not want to wake it?"

**Display in dashboards and API**: Every value returned by the API MUST include
its age. Stale values MUST be visually flagged. Expired values MUST be
prominently marked. A device that has been offline for 2 hours shows all its
data with "last updated 2h ago" -- the data is still useful (the device's
serial number has not changed, its chipset topology has not changed, its
last-known thermal state is informative) but consumers know to treat dynamic
values (temperature, bandwidth, power) with appropriate scepticism.

### Quality Decay

Measured values MUST decay according to the staleness policy of their refresh
class. Quality decay is the automatic mechanism that adjusts `InfoQuality`
based on the age of the data:

```
effective_quality = base_quality                if age < fresh_threshold
effective_quality = degrade(base_quality)       if age > stale_threshold
effective_quality = degrade(base_quality, 2)    if age > expired_threshold
```

The decay function MUST reduce quality by one level per threshold crossing:
`measured` to `reported` (stale), `reported` to `assumed` (expired). This
means routing decisions automatically become more conservative as data ages --
the router increases safety margins on stale values.

Re-measurement SHOULD be triggered proactively for stale values on active
pipelines -- if a link in a live video pipeline has stale bandwidth data,
the transport plugin is asked to re-measure before the data expires.

### Quality in Routing Decisions

The router MUST use quality levels in two ways:

1. **Confidence weighting**: When comparing two candidate pipelines, the one
   with higher-quality measurements MUST be preferred (all else being equal).
   A pipeline built on `measured` data is more trustworthy than one built on
   `assumed` data.

2. **Uncertainty budgeting**: Properties with `assumed` or `spec` quality MUST
   have a safety margin applied. If a USB 3.0 port's bandwidth is `spec`
   quality (5 Gbps), the router MUST treat it as 4 Gbps for capacity
   planning. If it is `measured` at 4.8 Gbps, the router MUST use 4.8 Gbps.
