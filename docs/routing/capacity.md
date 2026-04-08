# Device Capacity and Resource Pressure

**Status:** Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

Every device in the routing graph has finite resources. This document specifies
how the routing protocol models device capacity, tracks resource costs of
pipeline operations, enforces resource budgets on target machines, and governs
adaptive behaviour when devices are under pressure.

## Specification

### Capacity Model

Every device in the graph has finite resources. A Raspberry Pi has different
capacity from an Intel N100, which has different capacity from a workstation
with an RTX 4090. The routing protocol MUST track these limits so the router
does not overcommit a device -- and so software agents do not burden the machines
they run on.

```yaml
DeviceCapacity:
  resources: ResourcePool[]     # all resource types available on this device
  current_load: ResourceUsage   # what's being consumed right now
  reserved: ResourceUsage       # what's reserved by Ozma (active + warm pipelines)
  available: ResourceUsage      # capacity - current_load (what's actually free)
```

**ResourcePool** -- a single resource dimension with capacity and current state:

```yaml
ResourcePool:
  type: ResourceType            # what kind of resource
  capacity: float               # total available (units depend on type)
  current_usage: float          # currently in use (all consumers, not just Ozma)
  ozma_usage: float             # usage attributable to Ozma pipelines
  other_usage: float            # usage by non-Ozma processes
  quality: InfoQuality          # how we know these numbers
```

**ResourceType**:

```yaml
ResourceType: enum
  cpu_percent                   # CPU utilisation (0-100 per core, or total)
  cpu_cores                     # number of cores available
  memory_mb                     # RAM in megabytes
  gpu_percent                   # GPU compute utilisation
  gpu_memory_mb                 # GPU VRAM
  gpu_encode_slots              # hardware encoder sessions (NVENC, QSV, VCN)
  gpu_decode_slots              # hardware decoder sessions
  disk_iops                     # storage I/O operations per second
  disk_bandwidth_mbps           # storage throughput
  usb_bandwidth_bps             # aggregate USB bus bandwidth (per controller)
  network_bandwidth_bps         # network interface throughput
  thermal_headroom_c            # degrees below thermal throttle threshold
  power_rail_capacity_ma        # per-rail current capacity (see S2.10)
  power_rail_usage_ma           # per-rail current draw (measured or inferred)
```

Not every device reports every resource type. An SBC node MAY report CPU,
memory, and thermal headroom. A capture card reports nothing (it is a fixed-
function device -- its limits are expressed as port capabilities). A GPU codec
reports encode/decode slots. The router MUST work with whatever is available and
SHOULD apply `assumed` quality for missing data where it matters.

### Resource Cost of Pipeline Operations

Every pipeline operation consumes resources on the device it runs on. The
router MUST track this:

```yaml
ResourceCost:
  device_id: string             # which device is loaded
  costs: ResourceDemand[]       # per-resource-type demand

ResourceDemand:
  type: ResourceType
  active_cost: float            # resource consumed while pipeline is active
  warm_cost: float              # resource consumed while pipeline is warm
  peak_cost: float?             # transient peak during activation (e.g., codec init)
  quality: InfoQuality          # measured, estimated, or assumed
```

**Example resource costs**:

| Operation | Device | Resource | Active | Warm | Peak |
|-----------|--------|----------|--------|------|------|
| ffmpeg H.264 encode 1080p30 | Node (N100) | cpu_percent | 35% | 2% | 60% |
| ffmpeg H.264 encode 1080p30 | Node (N100) | memory_mb | 180 | 180 | 250 |
| NVENC H.264 encode 1080p60 | Workstation GPU | gpu_encode_slots | 1 | 1 | 1 |
| NVENC H.264 encode 1080p60 | Workstation GPU | gpu_percent | 5% | 0% | 10% |
| V4L2 capture 1080p60 | Node (Pi 5) | cpu_percent | 8% | 1% | 15% |
| V4L2 capture 1080p60 | Node (Pi 5) | usb_bandwidth_bps | 200M | 0 | 200M |
| VBAN audio stream | Node | cpu_percent | 1% | 0% | 2% |
| VBAN audio stream | Node | network_bandwidth_bps | 1.5M | 0 | 1.5M |
| Ozma desktop agent (idle) | Target PC | cpu_percent | 0.5% | -- | 2% |
| Ozma desktop agent (capture) | Target PC | cpu_percent | 8% | 1% | 15% |
| Ozma desktop agent (capture) | Target PC | memory_mb | 120 | 50 | 200 |
| PipeWire audio routing | Controller | cpu_percent | 1% | 0.5% | 3% |
| 3 warm preview pipelines | Node (Pi 5) | memory_mb | 540 | 540 | 540 |

Resource costs MAY be `assumed` (from a lookup table based on hardware class),
`spec` (from codec documentation), or `measured` (from actual observation). The
router SHOULD prefer `measured` and refine estimates over time.

### Resource Pressure and Routing Decisions

The router MUST use device capacity in three ways:

**1. Constraint checking**: The router MUST reject pipelines that would exceed
any device's capacity. This MUST be checked during path computation:

```
For each device touched by a candidate pipeline:
  For each resource type:
    if (device.current_usage + pipeline.resource_cost) > device.capacity:
      reject this pipeline
```

**2. Cost weighting**: Even when a device has headroom, high utilisation SHOULD
be penalised. A node at 80% CPU is a worse candidate for an additional encode
job than one at 20%:

```
cost(hop) += w_pressure * (device.current_usage / device.capacity)  # per resource type
```

This makes the router naturally load-balance across devices when multiple paths
exist.

**3. Warm pipeline eviction**: When device resources are contended, warm
pipelines MUST be evicted in priority order (lowest `warm_priority` first). The
router MUST cool pipelines until the device has headroom for the active workload.

### Resource Budgets and Agent Courtesy

Software agents running on target machines (desktop agent, soft node agent)
MUST NOT place an undue burden on the machine's primary workload. Resource
budgets on target machines MUST NOT exceed hard limits. The `resource_budget`
field on a device defines the maximum resources Ozma is allowed to consume:

```yaml
ResourceBudget:
  limits: ResourceLimit[]       # per-resource-type caps
  mode: budget_mode             # how strictly to enforce
  source: user | auto | default # who set this budget

ResourceLimit:
  type: ResourceType
  max_value: float              # absolute maximum Ozma may consume
  max_percent: float?           # alternative: percentage of total capacity
  backoff_threshold: float?     # start reducing at this level (soft limit)
  hard_limit: float             # never exceed this (hard limit)

BudgetMode: enum
  strict                        # hard enforcement -- reject pipelines that exceed budget
  adaptive                      # reduce quality/framerate to stay within budget
  advisory                      # report pressure but don't restrict (user monitors)
```

**Default budgets by device type**:

| Device type | Default mode | CPU | Memory | GPU encode | Rationale |
|-------------|-------------|-----|--------|------------|-----------|
| `controller` | strict | 80% | 80% | all slots | Dedicated to Ozma -- use most resources |
| `node` (SBC) | strict | 90% | 90% | all slots | Dedicated hardware -- use almost everything |
| `target` (workstation) | adaptive | 10% | 200 MB | 1 slot | User's machine -- be invisible |
| `target` (server) | adaptive | 15% | 500 MB | 2 slots | More headroom, but still secondary |
| `target` (kiosk) | strict | 30% | 500 MB | 2 slots | Dedicated purpose, but not Ozma's |

Users MAY override these defaults per device. A gaming PC's budget might be set
to 5% CPU (Ozma should be invisible during gaming) while a media server might
allow 25% (encoding is expected).

### Adaptive Budget Enforcement

When the budget mode is `adaptive`, the agent and router MUST collaborate to
stay within limits:

1. The agent MUST monitor its own resource consumption against its budget
2. If approaching `backoff_threshold`, the agent MUST signal the controller
3. The controller SHOULD degrade the pipeline: lower resolution, lower
   framerate, disable warm pipelines on this device
4. If the device's primary workload drops (user stops gaming), the agent
   SHOULD signal headroom available, and the controller SHOULD restore quality

This MUST be continuous, not one-shot. The agent MUST report resource pressure
periodically (via the existing device metrics system), and the router MUST
adjust pipeline parameters in response.

### Peak Load Protection

During activation (process startup, codec init), resource usage can spike
transiently above the steady-state cost. The `peak_cost` field captures this.
The router MUST check that the device can absorb the peak before activating a
pipeline -- even if steady-state usage is within budget, a startup spike that
starves the user's workload is unacceptable.

### Multi-Pipeline Resource Accounting

When multiple pipelines traverse the same device, their resource costs MUST be
summed. Three warm preview pipelines on a Pi 5 might each use 180 MB of memory
-- the router knows the device has 4 GB and MUST account for all three, not
just one. If a fourth pipeline would push memory over capacity, it MUST be
rejected or an existing warm pipeline MUST be evicted.

### Resource Discovery

Resource capacity MUST be discovered from the OS and hardware:

| Resource | Linux | Windows | macOS |
|----------|-------|---------|-------|
| CPU cores/speed | `/proc/cpuinfo`, `lscpu` | WMI | `sysctl` |
| Memory | `/proc/meminfo` | WMI | `sysctl` |
| GPU info | `nvidia-smi`, `vainfo`, `intel_gpu_top` | DXGI, NVAPI | `system_profiler` |
| GPU encode slots | Driver API (NVENC: `NvEncGetEncodeCaps`) | Same | VideoToolbox |
| USB bandwidth | `lsusb -t` (speed class) | WMI | `system_profiler` |
| Thermal | `sensors`, sysfs thermal zones | WMI | `powermetrics` |
| Current usage | `/proc/stat`, `nvidia-smi` | PDH counters | `host_processor_info` |

Current usage MUST be sampled periodically (default: every 5 seconds for idle
devices, every 1 second for devices with active pipelines). The reporting
interval SHOULD be adaptive -- more frequent when the device is under pressure,
less frequent when idle.
