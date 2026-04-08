# Thermal and Power Management

**Status:** Draft
**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in
this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document specifies the thermal and power management model for the Ozma
routing graph. The routing graph has temperature sensors, fan controls, power
consumption data, and the thermal topology linking them. This makes Ozma a
thermal and power management system -- not just monitoring, but active
control with full knowledge of the system's physical structure. This
specification defines fan curves, power profiles, thermal zones, and
intent-driven control, including thermal-aware routing decisions.

## Specification

### 1. Fan Curves

A fan curve maps a sensor reading to a fan speed. Ozma's model uses the
thermal topology: a zone has multiple sensors and multiple fans, and the
curve considers all of them.

```yaml
FanCurve:
  id: string
  name: string                  # "Silent", "Balanced", "Performance", "Full Speed"
  zone: string?                 # thermal zone this curve applies to (null = per-fan override)
  fan_ids: string[]?            # specific fans (null = all fans in zone)
  sensor_ids: string[]          # which sensors drive this curve
  sensor_mode: string           # "max" (hottest sensor wins), "average", "weighted"
  points: FanCurvePoint[]       # temperature -> speed mapping
  hysteresis_c: float?          # temperature hysteresis to prevent oscillation (default: 2 deg C)
  ramp_rate: float?             # max speed change per second (% per sec, prevents sudden jumps)
  min_duty_percent: float?      # minimum fan speed (never stop, or allow zero-RPM)
  max_duty_percent: float?      # maximum fan speed (cap below 100% for noise)
  critical_override: CriticalOverride?  # override to full speed at critical temp

FanCurvePoint:
  temp_c: float                 # temperature threshold
  duty_percent: float           # fan duty cycle (0-100)
  # Points are interpolated linearly between them.
  # Example: [{20, 25%}, {40, 35%}, {60, 60%}, {75, 100%}]

CriticalOverride:
  threshold_c: float            # above this: override to max regardless of curve
  shutdown_c: float?            # above this: emergency shutdown
  action: string                # "max_speed", "throttle_and_max", "shutdown"
```

Fan curve points MUST be interpolated linearly between defined points.
Implementations MUST support the `max`, `average`, and `weighted` sensor
modes. The default hysteresis SHOULD be 2 degrees C when not explicitly
specified.

The critical override MUST activate at critical temperature regardless of
fan curve. When `threshold_c` is reached, the fan MUST immediately operate
at maximum speed, ignoring the curve, ramp rate, and `max_duty_percent`
cap. When `shutdown_c` is reached and `action` is `"shutdown"`, the system
MUST initiate an emergency shutdown. The critical override MUST NOT be
disabled by user configuration.

**Comparison with BIOS / fancontrol:**

| Feature | BIOS / fancontrol | Ozma |
|---------|------------------|------|
| Sensor source | One sensor per fan | Multiple sensors per zone, weighted |
| Cause awareness | No -- just sees temperature number | Knows WHY temp is rising (I/O load, GPU render, ambient change) |
| Cross-zone coordination | No -- each fan independent | Zone-aware -- drive cage fans coordinate with exhaust fans |
| Predictive | No -- reactive only | Predictive -- "encode job starting, pre-ramp fans" |
| Profile switching | Manual (BIOS, software) | Automatic via intent bindings -- gaming = performance, idle = silent |
| Noise-aware | No | Links fan noise to room acoustics model -- adjusts during recording |
| Remote control | No | API-driven, fleet-wide, per-zone |
| Redundancy-aware | No | Knows N+1 status -- compensates for failed fan |
| Power-aware | No | Knows fan power draw, adjusts within power budget |

### 2. Intent-Driven Fan Profiles

Fan curves are tied to intents. Power profiles SHOULD switch automatically
with intent. When the scenario switches to `gaming`, the fan profile SHOULD
switch to "Performance". When idle, "Silent". When recording audio
(`creative` intent), noise-sensitive scenarios SHOULD cap fan speed based
on microphone distance -- the system knows the microphone's distance from
each fan and the fan's noise output at each speed.

Implementations MUST support associating fan curves with intents.
Implementations SHOULD pre-ramp fans predictively when a workload change
is detected (e.g., game launching) rather than waiting for temperature to
rise.

### 3. Power Profiles

Power profiles control CPU/GPU frequency, voltage, and power limits. Ozma
unifies per-vendor tools (Ryzen Master, Intel XTU, NVIDIA GPU Tweak) with
the same intent-driven model.

```yaml
PowerProfile:
  id: string
  name: string                  # "Power Saver", "Balanced", "Performance",
                                # "Max Performance", "Silent"
  cpu: CpuPowerConfig?
  gpu: GpuPowerConfig[]?        # per-GPU
  platform: PlatformPowerConfig?

CpuPowerConfig:
  governor: string?             # "performance", "powersave", "schedutil", "conservative"
  min_freq_mhz: uint?           # minimum CPU frequency
  max_freq_mhz: uint?           # maximum CPU frequency (cap below max for power/heat)
  tdp_limit_w: float?           # PL1 / PPT power limit
  boost_limit_w: float?         # PL2 / PBO boost power limit
  boost_enabled: bool?          # allow turbo/boost clocks
  core_parking: bool?           # park idle cores (Windows)
  epp: string?                  # Energy Performance Preference (intel)
                                # "performance", "balance_performance",
                                # "balance_power", "power"
  amd_pbo: PboConfig?           # AMD Precision Boost Overdrive settings
  undervolt_mv: int?            # undervolt offset (negative = less voltage = less heat)

PboConfig:
  mode: string                  # "disabled", "enabled", "advanced"
  ppt_limit_w: float?           # Package Power Tracking limit
  tdc_limit_a: float?           # Thermal Design Current limit
  edc_limit_a: float?           # Electrical Design Current limit
  curve_optimizer: int?         # per-core curve optimizer offset (negative = undervolt)
  max_boost_override_mhz: int?  # additional boost clock offset

GpuPowerConfig:
  device_id: string             # which GPU
  power_limit_w: float?         # GPU power limit (percentage or absolute)
  core_clock_offset_mhz: int?   # core clock offset
  memory_clock_offset_mhz: int? # memory clock offset
  fan_curve: FanCurve?          # GPU-specific fan curve (overrides zone default)
  performance_mode: string?     # "max_performance", "balanced", "quiet",
                                # "power_saver" (vendor-specific)

PlatformPowerConfig:
  usb_suspend: bool?            # USB selective suspend (saves power, can cause device issues)
  pcie_aspm: string?            # "disabled", "l0s", "l1", "l0s_l1" -- PCIe power saving
  sata_alpm: string?            # "disabled", "min_power", "medium_power", "max_performance"
  display_sleep_min: uint?      # display sleep timeout
  disk_sleep_min: uint?         # disk spindown timeout
  wake_on_lan: bool?            # WoL (must stay enabled for Ozma remote wake)
```

When `wake_on_lan` is set to `false`, implementations MUST warn the operator
that Ozma remote wake functionality will be unavailable.

**Intent to power profile mapping (configurable per scenario):**

```yaml
# Intent -> power profile mapping (configurable per scenario)
IntentPowerMapping:
  gaming:       "Performance"     # max clocks, boost enabled, fans aggressive
  creative:     "Balanced"        # good performance, moderate noise
  desktop:      "Balanced"
  fidelity_audio: "Silent"       # cap CPU, cap fans, minimum noise for recording
  observe:      "Power Saver"    # minimal power when just monitoring
  control:      "Power Saver"    # headless -- no display, minimal clocks
  preview:      "Power Saver"
```

Power profiles SHOULD switch automatically with intent changes. The mapping
MUST be configurable per scenario. Implementations MUST NOT change power
profiles without an explicit intent change or manual override.

### 4. Thermal-Aware Routing

The routing graph SHOULD use thermal data as an input to routing decisions:

1. **Encode job placement**: GPU at 85 deg C and throttling -- the router
   SHOULD route encode to iGPU (Quick Sync) or CPU (software) instead.
   The router SHOULD check thermal headroom before placing
   compute-intensive pipeline operations.

2. **Storage path selection**: NVMe at 70 deg C and throttling -- if the
   system has a second NVMe or SATA drive with thermal headroom, the router
   SHOULD route recording there instead.

3. **Predictive fan ramp**: Intent binding detects "game launching" -- the
   system SHOULD pre-ramp fans before the GPU load arrives. The system
   SHOULD NOT wait for temperature to rise -- it knows the thermal
   consequence of the workload and SHOULD act proactively.

4. **Noise-sensitive scenarios**: `creative` or `fidelity_audio` intent --
   the system SHOULD cap fans to keep ambient noise below microphone
   sensitivity threshold. The system knows: mic sensitivity (from
   AudioSpec), mic distance from each fan (from PhysicalLocation), fan
   noise at each speed (from FanSpec). It SHOULD compute the maximum fan
   speed that keeps fan noise below the mic's self-noise floor at the
   mic's position.

5. **Power budget enforcement**: PSU at 90% capacity -- the system SHOULD
   reduce GPU power limit to create headroom. UPS on battery -- the system
   SHOULD switch to "Power Saver" profile to extend runtime.

### 5. Observability

```
GET /api/v1/thermal/zones                   # all thermal zones with current temps + fan speeds
GET /api/v1/thermal/zones/{id}              # zone detail with sensor history
GET /api/v1/thermal/fans                    # all fans with RPM, duty, power draw
GET /api/v1/thermal/profiles                # available fan profiles
PUT /api/v1/thermal/profiles/{id}           # create/update fan profile
POST /api/v1/thermal/profiles/{id}/activate # switch active profile
GET /api/v1/power/profiles                  # available power profiles
PUT /api/v1/power/profiles/{id}             # create/update power profile
POST /api/v1/power/profiles/{id}/activate   # switch active profile
GET /api/v1/power/state                     # current CPU/GPU clocks, voltages, power draw
```

Implementations MUST expose thermal zone and fan data via the REST API.
Implementations MUST support creating, updating, and activating both fan
profiles and power profiles via the API.

### 6. Events

```
thermal.zone.warning             # zone temperature approaching threshold
thermal.zone.critical            # zone at critical temperature
thermal.zone.recovered           # zone returned to normal
thermal.fan.failed               # fan RPM dropped to zero or below minimum
thermal.fan.degraded             # fan not reaching target speed
thermal.profile.switched         # fan/power profile changed (by intent or user)
power.profile.switched           # power profile changed
power.throttle.active            # CPU or GPU thermal throttling detected
power.throttle.cleared           # throttling ended
```

Implementations MUST emit `thermal.zone.critical` when a zone reaches
critical temperature. Implementations MUST emit `thermal.fan.failed` when
a fan's RPM drops to zero or below the configured minimum. Implementations
MUST emit `power.throttle.active` when CPU or GPU thermal throttling is
detected. Implementations SHOULD emit `thermal.zone.warning` when a zone
temperature approaches the threshold.
