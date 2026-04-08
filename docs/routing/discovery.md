# Topology Discovery

**Status:** Draft

**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document specifies the topology discovery system for the Ozma routing
engine. Topology discovery builds the graph from what the OS and devices report.
It runs continuously — initial discovery on startup, then hotplug-driven updates.
The specification covers the five discovery layers (hardware, OS, network,
measurement, enrichment), handling of opaque devices, compound device
decomposition, and calibration probes that replace assumed data with measured
data.

---

## Specification

### 1. Discovery Layers

Discovery MUST proceed in layers, from physical to logical. Each layer builds on
the data produced by the previous layer.

**Layer 1: Hardware Enumeration**

Each device plugin MUST discover its device class:

- USB: enumerate controllers, hubs, devices, their tree structure and speeds
- Thunderbolt: enumerate dock topology, internal USB hubs, display outputs
- PCI: enumerate capture cards, GPUs, network cards
- Network: enumerate interfaces, link speeds, routing tables

This produces devices and ports with `reported` quality properties.

**Layer 2: OS Interface Mapping**

The system MUST map hardware to OS-level interfaces:

- USB capture card to V4L2 device node to resolution/framerate capabilities
- USB audio device to ALSA device to PipeWire node to sample rate/channel capabilities
- USB HID device to `/dev/hidgN` to report descriptor to HID format
- Network interface to IP address to reachability to other Ozma nodes

This enriches ports with capability information at `reported` quality.

**Layer 3: Network Topology**

The system MUST discover other Ozma devices on the network:

- mDNS discovery to node inventory
- Direct registration (for nodes behind NAT/SLIRP)
- Connect relay topology (for remote nodes)

This creates cross-machine links with `reported` quality for network properties.

**Layer 4: Active Measurement**

The system SHOULD probe links to get `measured` quality data:

- Bandwidth: send probe packets, measure throughput
- Latency: RTT measurement (subtract processing time)
- Jitter: statistical analysis of packet timing
- Loss: count sent vs received over a window

Active measurement MUST run periodically on standby links and continuously
(passive observation) on active links.

**Layer 5: Capability Enrichment**

The system MUST combine information from multiple sources:

- USB capture card reports 1080p60 capability (V4L2 enumeration, `reported`)
- USB bus is USB 2.0 (sysfs, `reported`) — bandwidth limited to approximately 280 Mbps
- Measured throughput is 240 Mbps (`measured`)
- Therefore: effective capability is 1080p30 MJPEG, not 1080p60 raw
  (derived from combining `reported` + `measured` data)

This is where information quality becomes critical. The capture card *says* it
can do 1080p60, but the USB bus cannot carry it uncompressed. The router MUST
combine multiple quality-tagged properties to compute effective capabilities.

### 2. Opaque Devices

Some devices are partially or fully opaque — their internal topology cannot be
discovered. Thunderbolt docks are the canonical example: the dock MAY use USB 2.0
internally for its hub while advertising USB 3.0 on its external ports.

**Strategy for opaque devices**:

1. Report what the OS tells us (`reported` quality).
2. Apply `assumed` defaults for unknown internals (e.g., assume dock USB hub
   matches external port speed). Opaque device properties MUST be tagged with
   `assumed` quality.
3. Measure actual throughput to override assumptions (`measured`).
4. Allow user override (`user` quality) for anything the system gets wrong.

Over time, the device database MAY accumulate known internal topologies for
specific dock models (by VID/PID), upgrading `assumed` to `spec`.

### 3. Compound Device Decomposition

Compound devices (docks, KVM cables, USB hubs with integrated audio) MUST be
decomposed into sub-devices with internal links:

```
Thunderbolt Dock (VID:PID 0x1234:0x5678)
+-- USB Hub (internal)
|   +-- Port: usb-downstream-1 (USB 3.0, reported)
|   +-- Port: usb-downstream-2 (USB 3.0, reported)
|   +-- Port: usb-upstream (Thunderbolt, reported)
|       +-- Internal Link -> Thunderbolt Controller
+-- Ethernet Adapter (internal)
|   +-- Port: ethernet (1 Gbps, reported)
|   +-- Port: usb-upstream (USB 3.0, reported)
|       +-- Internal Link -> USB Hub
+-- DisplayPort MST Hub (internal)
|   +-- Port: dp-out-1
|   +-- Port: dp-out-2
|   +-- Port: thunderbolt-upstream
|       +-- Internal Link -> Thunderbolt Controller
+-- Thunderbolt Controller
    +-- Port: thunderbolt-upstream (40 Gbps, reported)
        +-- External Link -> Host Thunderbolt Port
```

The router MUST see all of these as part of the graph. If a capture card is
connected to `usb-downstream-1`, the router traces the path: capture card to
USB hub to Thunderbolt controller to host — and knows the bottleneck is the
USB hub's internal bandwidth, not the Thunderbolt link.

### 4. Topology Calibration

When a device's internal topology is unknown or only partially known (the
dock example above — the OS reports "USB 3.0" on all downstream ports, but
some MAY actually be USB 2.0 internally), the system MAY run a **calibration
probe** to replace `assumed` or `spec` data with `measured` data.

**Calibration probe protocol**:

```yaml
CalibrationProbe:
  target: DeviceRef             # device to calibrate
  probe_type: string            # type of probe to run
  state: string                 # "pending", "running", "complete", "failed"
  results: CalibrationResult[]  # per-link/port results
  triggered_by: string          # "auto_discovery", "user_request", "schedule"
  disruptive: bool              # does this probe disrupt active pipelines?

CalibrationResult:
  entity: string                # port or internal link ID
  property: string              # what was measured ("bandwidth_bps", "latency_ms", "usb_speed")
  before: QualifiedValue        # previous value + quality
  after: QualifiedValue         # measured value + quality
  contributed_to_db: bool       # was this result submitted to the device database?
```

**Probe types**:

| Probe type | What it measures | Disruptive? | How |
|-----------|-----------------|-------------|-----|
| `usb_bandwidth` | Actual throughput per USB port | Yes — sends bulk data | Loopback transfer test via gadget or known device |
| `network_bandwidth` | Link throughput | Minimally — background traffic | iperf-style probe between endpoints |
| `latency` | Per-hop round-trip time | No | Timing probes on existing or probe traffic |
| `usb_speed_class` | Actual USB speed (not just reported) | No | Read from sysfs after device connection |
| `power_delivery` | Actual available current per port | Yes — draws increasing load | Controlled current draw test (requires INA219 or PD) |
| `display_edid` | Display capabilities | No | Read EDID from connected display |
| `audio_loopback` | Audio path latency and quality | Yes — sends test tone | Loopback measurement through audio pipeline |

**Auto-calibration triggers**:

- New unknown device detected (no device database match) — the system SHOULD
  probe USB speed class and bandwidth.
- Device database entry has `confidence: "assumed"` or `"estimated"` for
  internal topology — the system SHOULD offer calibration.
- User plugs a capture card into a new port — the system SHOULD probe that
  port's actual bandwidth.

**Results feed back into the device database**. When a user calibrates a
specific dock model and discovers its internal USB hub is actually USB 2.0
on ports 3-4, that result SHOULD be submitted to Connect. Future users with the
same dock (matched by VID/PID) get the corrected topology automatically —
no calibration needed. Calibration results SHOULD be submitted to the device
database to benefit the community.

**API**:

```
POST /api/v1/routing/calibrate/{device_id}          # start calibration
GET  /api/v1/routing/calibrate/{device_id}/status    # probe status
GET  /api/v1/routing/calibrate/{device_id}/results   # probe results
POST /api/v1/routing/calibrate/{device_id}/submit    # submit results to device database
```
