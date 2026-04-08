# Control Path

**Status:** Draft
**RFC 2119 Conformance:** The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in
this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This document specifies the control path model for the Ozma routing graph.
The routing graph has two planes: the data plane (how media flows -- video,
audio, HID, RGB) and the control plane (how commands reach devices). While
the data plane is modelled in detail elsewhere, the control plane is equally
critical -- if the command cannot reach the device, the device cannot be
managed. This specification defines how control paths are described,
how reachability is tracked, and how the router uses control path status
to gate pipeline activation and failover planning.

## Specification

### 1. Control Path Model

Every controllable device in the routing graph MUST have a control path.
A control path describes how a command gets from the controller to a device.

Unlike data plane links which are always point-to-point between ports,
control paths MAY traverse intermediaries, use out-of-band channels, or
require specific physical connections.

### 2. Schema

```yaml
ControlPath:
  method: ControlMethod         # how commands are delivered
  reachability: Reachability    # can the controller currently reach this device?
  latency_ms: float?            # typical command round-trip time
  quality: InfoQuality

ControlMethod:
  type: string                  # transport type for control commands
  via: ControlVia               # what the command travels through
  protocol: string?             # application-level protocol
  address: string?              # how to reach it (IP, serial port, BLE address, etc.)
  credentials: string?          # credential reference (not the credential itself)
  fallback: ControlMethod?      # alternative control path if primary fails

ControlVia: enum
  direct                        # controller talks directly to device (IP, USB, local bus)
  proxy                         # controller talks through another device
  cloud                         # controller talks through a cloud service
  physical                      # requires physical human action (button press)
```

Implementations MUST support all four `ControlVia` values. The `credentials`
field MUST contain a reference identifier, and MUST NOT contain the credential
itself.

### 3. Control Path Types

| Control type | Via | Example | Dependency |
|-------------|-----|---------|-----------|
| IP direct | `direct` | WLED HTTP API, SNMP | Network reachability |
| IP via cloud | `cloud` | UniFi Cloud Controller, Hue Cloud | Internet + vendor cloud |
| IP via local controller | `proxy` | UniFi via Cloud Key on LAN | Cloud Key must be running |
| Serial | `direct` | HDMI matrix via RS-232 | Serial cable + specific machine |
| Serial via node | `proxy` | HDMI matrix via USB-serial on Node 3 | Node 3 must be online |
| CEC | `direct` | TV via HDMI cable | Physically connected via HDMI |
| CEC via node | `proxy` | TV via HDMI, controlled from node that has the HDMI output | Node + HDMI cable |
| DDC/CI | `direct` | Monitor via display cable | Physically connected via DP/HDMI |
| DDC/CI via agent | `proxy` | Monitor DDC/CI via desktop agent on connected PC | Agent must be running |
| BLE | `direct` | Desk controller via BLE | BLE adapter in range |
| BLE via node | `proxy` | Desk controller via BLE on nearest node | Node with BLE + proximity |
| GPIO | `direct` | LoM relay on node GPIO | Specific node's GPIO |
| IR | `direct` | IR blaster attached to node | Node + IR hardware + line of sight |
| MQTT | `proxy` | IoT device via MQTT broker | Broker must be running |
| QMP | `direct` | QEMU VM via QMP socket | Hypervisor host + socket access |
| API via agent | `proxy` | Target machine feature via desktop agent | Agent running on target |
| Manual | `physical` | Physical button on device | Human present |

### 4. Reachability

```yaml
Reachability:
  status: reachable | unreachable | degraded | unknown
  last_contact: timestamp?      # when we last successfully communicated
  failure_reason: string?       # if unreachable: why
  dependent_on: DeviceRef[]     # devices that must be online for this path to work
```

The `dependent_on` field MUST list all devices that are REQUIRED for the
control path to function. The router MUST evaluate the reachability of every
device in `dependent_on` when determining whether a control path is reachable.

**Worked example -- dependency chain:**

```
Controller
  -> can control: WLED strip (direct, IP)
  -> can control: Node 3 (direct, IP)
  -> can control: HDMI matrix (via Node 3, serial)
     dependent_on: [node-3]
  -> can control: Monitor brightness (via Desktop Agent on Workstation A, DDC/CI)
     dependent_on: [agent-workstation-a]
  -> can control: UniFi switch (via Cloud Key, API)
     dependent_on: [unifi-cloud-key]
  -> can control: TV power (via Node 2, CEC over HDMI)
     dependent_on: [node-2]
```

If Node 3 goes offline, the controller loses control of the HDMI matrix --
even though the matrix itself is working. The router knows this because
`dependent_on: [node-3]` is explicit.

### 5. Router Behaviour

#### 5.1 Remediation

When remediation requires switching a device, the router MUST check whether
the control path to that device is reachable before attempting the switch.

#### 5.2 Pipeline Activation

The router MUST check control path reachability before pipeline activation.
If activating a pipeline requires switching an externally-controlled device,
and the control path to that device is unreachable, the router MUST reject
that pipeline.

#### 5.3 Failover Planning

The router SHOULD pre-compute the impact of each control proxy going
offline. For example: "If Node 3 dies, we lose control of the HDMI matrix
and the serial console." This data feeds into the diagnostic/simulation API.

#### 5.4 Redundant Control Paths

Some devices have multiple control paths (e.g., a TV controllable via CEC
from Node 2 OR via IP API directly). The `fallback` field on `ControlMethod`
expresses this. If the primary path fails, the router SHOULD attempt the
fallback path automatically.

### 6. Graph Integration

Control paths MUST NOT be modelled as links in the data plane graph -- they
do not carry media, they do not have bandwidth or jitter. However, the router
MUST consider control path reachability during pipeline assembly: a pipeline
that requires switching an externally-controlled device is only feasible if
the control path to that device is reachable.

### 7. Cloud Control Path Risks

When a device is controlled via a cloud service (`via: cloud`), the control
path depends on internet connectivity and the vendor's cloud availability.
The router SHOULD surface this as a dependency and SHOULD warn that loss of
internet connectivity will result in loss of control over those devices.
The router SHOULD recommend local control alternatives where they exist.
Devices with only cloud control paths SHOULD be flagged in the health
dashboard.

### 8. Events

```
control.path.reachable          # control path became reachable
control.path.unreachable        # control path lost (dependency down, network issue)
control.path.degraded           # control path latency increased significantly
control.path.fallback_activated # primary control path failed, using fallback
```

Implementations MUST emit `control.path.unreachable` when a control path
transitions to unreachable status. Implementations MUST emit
`control.path.fallback_activated` when a fallback control method is engaged.
Implementations SHOULD emit `control.path.degraded` when control path latency
increases significantly above the baseline.
