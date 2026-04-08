# Device Versioning and Mesh Updates

**Status**: Draft
**RFC 2119 Conformance**: The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

---

## Abstract

This document specifies the version model, firmware tracking, update delivery,
orchestration, and observability for all Ozma-managed devices in the routing
graph. Every device -- controllers, hardware nodes, soft nodes, desktop agents,
screen firmware, RGB controllers -- has a software or firmware version that the
routing protocol tracks and that affects routing decisions.

---

## Specification

### 1. Version Model

Every device in the graph MUST report its version using the following model:

```yaml
DeviceVersion:
  # --- Ozma software version (for Ozma-managed components) ---
  component: string?            # "controller", "node", "agent", "softnode",
                                # "screen_firmware", "rgb_firmware", "plugin"
  current_version: SemVer?      # currently running version
  channel: string?              # "stable", "beta", "nightly", "pinned"
  platform: string?             # "linux-amd64", "linux-arm64", "linux-riscv64",
                                # "windows-amd64", "macos-arm64", "esp32", "rp2040"
  build_info: BuildInfo?        # build metadata
  update_state: UpdateState     # current update status
  protocol_version: string?     # ozma protocol version this device speaks ("ozma/0.1")
  min_compatible: string?       # minimum controller version this device works with
  max_compatible: string?       # maximum controller version (if known)

  # --- Third-party device firmware (for any device with updateable firmware) ---
  firmware: FirmwareInfo[]?     # firmware components on this device (may be multiple)

FirmwareInfo:
  component: string             # what firmware this is
  current_version: string?      # running firmware version (as reported by device)
  vendor: string?               # firmware vendor
  device_type: string?          # LVFS device type / GUID
  updatable: bool               # can this firmware be updated?
  update_method: FirmwareUpdateMethod?  # how to update it
  update_state: UpdateState     # current update status
  known_issues: FirmwareIssue[]?  # known bugs in current version
  history: FirmwareVersion[]?   # version history (from device database or LVFS)

FirmwareUpdateMethod:
  mechanism: string             # "fwupd", "vendor_tool", "usb_dfu", "ota_http",
                                # "via_qmk", "bluetooth_ota", "serial_flash",
                                # "bios_flash", "manual_rom", "not_updatable"
  lvfs: bool                    # available via LVFS (Linux Vendor Firmware Service)
  lvfs_guid: string?            # LVFS device GUID for update matching
  vendor_url: string?           # vendor's firmware download page
  requires: string?             # special requirements ("windows_only", "vendor_app",
                                # "usb_cable", "bluetooth", "bios_menu")
  risk: string                  # "safe", "low", "medium", "high", "brick_risk"
  # "safe" = automatic, reliable rollback (fwupd with capsule update)
  # "low" = well-tested process, rare failures
  # "medium" = vendor tool required, some failure reports
  # "high" = manual process, failure = RMA or recovery mode
  # "brick_risk" = no recovery if interrupted (some keyboard MCUs, old SSDs)

FirmwareVersion:
  version: string
  date: string?                 # release date
  changelog: string[]?          # what changed
  known_issues: FirmwareIssue[]?
  known_fixes: string[]?        # issues fixed in this version
  lvfs_release: string?         # LVFS release ID (if available)
  source: string?               # "lvfs", "vendor", "community"

FirmwareIssue:
  id: string
  severity: string              # "critical", "major", "minor", "cosmetic"
  category: string              # subsystem affected
  summary: string
  description: string?
  workaround: string?
  fixed_in: string?             # firmware version that fixes it
  cve: string?                  # CVE if security-related
  affects_ozma: bool?
  ozma_impact: string?

BuildInfo:
  commit: string?               # git commit hash
  build_date: timestamp?        # when this build was produced
  edition: string?              # "open_source", "free", "commercial"
  signature: string?            # Ed25519 signature (base64)
  signed_by: string?            # signing key identifier

UpdateState:
  status: up_to_date | update_available | updating | update_failed | unknown
  available_version: string?    # newest version available (null if up_to_date or unknown)
  available_channel: string?    # channel of available update
  last_check: timestamp?        # when we last checked for updates
  last_update: timestamp?       # when the device was last updated
  failure_reason: string?       # if update_failed, why
  can_update: bool              # does this device support remote update?
  requires_reboot: bool?        # will the update require a restart?
  rollback_available: bool?     # can this device roll back to previous version?
```

### 2. Firmware Coverage

The following table lists device classes, their firmware components, update
methods, and LVFS coverage. Implementations MUST enumerate firmware for all
device classes that are present in the routing graph.

| Device class | Firmware components | Update method | LVFS coverage |
|-------------|-------------------|---------------|---------------|
| Motherboard | BIOS/UEFI | BIOS flash, fwupd capsule | Good (many vendors) |
| CPU | Microcode | Loaded by OS or BIOS | Via BIOS update |
| GPU | VBIOS, driver firmware | Vendor tool, fwupd | Partial (some NVIDIA/AMD) |
| SSD/NVMe | Controller firmware | fwupd, vendor tool | Good (Samsung, WD, Intel, Crucial) |
| HDD | Controller firmware | Vendor tool | Limited |
| Thunderbolt dock | Thunderbolt controller FW, USB hub FW, PD controller FW | fwupd, vendor tool | Good (CalDigit, Lenovo, Dell) |
| USB hub | Hub controller firmware | Vendor tool (rare) | Rare |
| Monitor | Scaler firmware | OSD menu, USB, vendor app | Rare (Dell via fwupd) |
| Keyboard (QMK/VIA) | MCU firmware | QMK DFU, VIA | No (community tooling) |
| Keyboard (vendor) | MCU firmware | Vendor app (iCUE, Synapse, G Hub) | No |
| Mouse | Sensor + wireless firmware | Vendor app | No |
| Headset | DSP firmware, Bluetooth firmware | Vendor app | Rare |
| Webcam | ISP firmware | fwupd, vendor tool | Partial (Logitech) |
| Network card | NIC firmware | fwupd, ethtool flash | Good (Intel, Mellanox) |
| WiFi card | WiFi firmware | fwupd, kernel firmware | Good (Intel) |
| Bluetooth adapter | BT firmware | fwupd, kernel firmware | Partial |
| WLED controller | ESP firmware | OTA HTTP | No (Ozma manages directly) |
| Stream Deck | MCU firmware | Elgato app, fwupd | Partial |
| Printer | Controller firmware | Vendor tool | Partial (HP, Lexmark) |
| UPS | Controller firmware | NUT, vendor tool | Rare |
| Smart PSU | Controller firmware | Vendor app (Corsair Link, etc.) | No |
| USB-C PD controller | PD firmware | fwupd | Partial (TI, Cypress/Infineon) |

### 3. LVFS / fwupd Integration

LVFS (Linux Vendor Firmware Service) is the primary data source for
third-party firmware versions and updates. Implementations MUST use `fwupd`
on Linux as the primary firmware discovery and update mechanism.

```yaml
LvfsIntegration:
  # The controller/agent queries fwupd for firmware state of all devices
  discovery: string             # "fwupdmgr get-devices" -- lists all fwupd-visible devices
                                # with current version, update availability, and GUID
  update_check: string          # "fwupdmgr get-updates" -- available updates from LVFS
  history: string               # "fwupdmgr get-history" -- past update attempts + results

  # Mapping to the routing graph:
  # For each fwupd device:
  #   1. Match to graph device via USB VID/PID, PCI ID, or device path
  #   2. Populate FirmwareInfo.current_version from fwupd
  #   3. Populate FirmwareInfo.update_state from LVFS metadata
  #   4. Populate FirmwareInfo.known_issues from device database
  #   5. Cross-reference with device database for Ozma-specific impact
```

On Windows, the agent SHOULD query Windows Update for driver/firmware versions
and check vendor APIs where available. On macOS, the agent SHOULD use
`system_profiler` for firmware versions of Apple hardware and supported
third-party devices.

### 4. Firmware as a Routing Concern

Firmware versions affect device capabilities and reliability. The routing graph
MUST account for firmware because:

1. **Known bugs affect routing quality**: A Thunderbolt dock with firmware v1.2
   that has a known USB dropout bug SHOULD have its links marked as lower
   reliability until firmware is updated.

2. **New firmware enables new capabilities**: A monitor firmware update that adds
   VRR support, or a NIC firmware update that enables 2.5GbE on hardware that
   previously only ran at 1GbE, SHOULD be reflected in the device's capability
   enumeration.

3. **Security vulnerabilities**: Firmware CVEs (Thunderbolt Spy, SSD encryption
   bypass, NIC remote code execution) MUST be surfaced. The agent detects the
   firmware version, the database flags the CVE, and the dashboard shows the
   warning.

4. **Fleet firmware management**: For managed deployments, firmware versions
   across all devices in the fleet SHOULD be visible.

BIOS known issues SHOULD be surfaced when they affect the running configuration.

### 5. Firmware Observability

Implementations MUST expose the following API endpoints for firmware
observability:

```
GET /api/v1/firmware/devices             # all devices with firmware info
GET /api/v1/firmware/devices/{id}        # firmware detail for one device
GET /api/v1/firmware/updates-available   # all devices with pending firmware updates
GET /api/v1/firmware/issues              # all known firmware issues affecting current fleet
GET /api/v1/firmware/history             # firmware update history
POST /api/v1/firmware/check              # trigger firmware update check (fwupd refresh)
POST /api/v1/firmware/update/{id}        # apply firmware update (if safe + user approves)
```

Implementations MUST emit the following events:

```
firmware.update_available       # new firmware detected for a device
firmware.update_applied         # firmware successfully updated
firmware.update_failed          # firmware update failed
firmware.issue_detected         # known issue affects a device's current firmware
firmware.security_advisory      # CVE affects a device's firmware
```

### 6. Version in the Routing Graph

Versions are a property of devices, not links. They affect routing in the
following ways:

**Protocol compatibility**: A node running `ozma/0.1` MAY not support features
added in `ozma/0.2`. The router MUST check `protocol_version` and
`min_compatible`/`max_compatible` when assembling pipelines. If a node cannot
speak the required protocol version for a transport or format, the pipeline
MUST be rejected or a compatible fallback MUST be selected.

**Feature capabilities**: Newer versions MAY support additional formats, codecs,
or transport types. The router MUST discover these via capability enumeration --
version is informational, capabilities are authoritative.

**Fleet consistency**: When devices in the mesh are at different versions, the
controller MUST track this as a health indicator. Mixed-version deployments
MUST be supported (backwards compatibility is REQUIRED within a major version),
but the controller SHOULD surface version drift as a warning.

### 7. Update Delivery Through the Mesh

Updates are delivered through the existing mesh infrastructure. The controller
acts as the update coordinator -- it knows what versions are available, what
each device is running, and orchestrates the update process.

#### 7.1 Update Sources

```yaml
UpdateSource:
  type: string                  # "connect", "local", "url", "manual"
  url: string?                  # for "connect": Connect API; for "url": direct HTTP
  check_interval_s: uint        # how often to check (default: 3600 = hourly)
  auto_update: bool             # automatically apply updates without user confirmation
  auto_update_window: TimeWindow? # only auto-update during this window (e.g., 02:00-05:00)
```

| Source | How it works | When |
|--------|-------------|------|
| `connect` | Controller checks Connect API for new versions | Default for registered controllers |
| `local` | Controller serves updates from its local filesystem | Air-gapped deployments |
| `url` | Controller fetches from a configured URL | Self-hosted update mirror |
| `manual` | User uploads update file via API | Emergency or testing |

#### 7.2 Update Flow

The controller MUST follow this sequence when applying updates:

1. Controller checks update source for new versions.
2. For each device type with an available update:
   a. Check compatibility (protocol version, platform, dependencies).
   b. Download update artefact (if not already cached).
   c. Verify integrity (SHA-256) and authenticity (Ed25519 signature).
      Implementations MUST NOT apply an update that fails either check.
   d. If `auto_update` is enabled: proceed. Otherwise: notify user, wait for
      approval.
3. For each device to update:
   a. Check device health and resource state (implementations MUST NOT update
      a device under resource pressure).
   b. For nodes: the controller MUST ensure no active pipeline depends solely
      on this node. If the node is the sole active KVM target, the controller
      MUST NOT proceed without explicit user approval or a verified failover
      path.
   c. Push update to device via the mesh (binary channel, spec 09, or HTTP).
   d. Device applies update (A/B partition, container restart, process restart).
   e. Device reports new version on reconnection.
   f. Controller verifies capabilities and re-runs graph discovery.
4. If update fails:
   a. Device MUST roll back (if rollback is supported).
   b. Controller MUST mark device as `update_failed` with reason.
   c. Controller MUST alert user.

#### 7.3 Update Types by Device Class

| Device class | Update mechanism | Disruption | Rollback |
|-------------|-----------------|------------|----------|
| Controller | Process restart or container update | Brief API outage (<5s) | Previous container/binary |
| Hardware node (SBC) | A/B partition flash + reboot | Node offline during reboot (~30s) | Boot to previous partition |
| Soft node | Process restart | Node offline during restart (~2s) | Previous binary |
| Desktop agent | Process restart (background) | No disruption to user | Previous binary |
| Screen firmware (ESP32) | OTA flash via HTTP | Screen dark during flash (~30s) | Dual-partition if supported |
| RGB controller (WLED) | OTA flash via WLED API | LEDs off during flash (~15s) | WLED has its own rollback |
| Plugins | Hot-reload if possible, restart if not | Depends on plugin | Previous version |

### 8. Update Orchestration

Updates across a fleet MUST be orchestrated, not applied simultaneously.

**Rolling updates**: Nodes MUST be updated one at a time (or in configurable
batch sizes). The controller MUST wait for each node to come back healthy
before proceeding. If a node fails to return, the rollout MUST be paused and
the user MUST be alerted.

**Dependency ordering**: If an update requires a minimum controller version,
the controller MUST update itself first. If nodes require a minimum agent
version on their targets, agents MUST be updated before nodes.

**Pipeline-aware scheduling**: The controller MUST NOT update a node that is
currently the sole active KVM target without explicit user approval or a
verified failover path. For non-critical nodes (preview-only, monitoring),
updates MAY proceed automatically.

**Maintenance windows**: Auto-updates MAY be restricted to a time window
(`auto_update_window`). Outside the window, updates MUST be downloaded and
staged but MUST NOT be applied until the window opens.

### 9. Version Observability

Implementations MUST expose the following API endpoints:

```
GET /api/v1/devices/versions              # all devices with current version info
GET /api/v1/devices/{id}/version          # single device version detail
GET /api/v1/updates/available             # all available updates across the fleet
POST /api/v1/updates/check                # force an update check now
POST /api/v1/updates/apply                # apply pending updates (with options)
POST /api/v1/updates/apply/{device_id}    # update a specific device
GET /api/v1/updates/history               # update history (who, when, from->to, success/fail)
```

Implementations MUST emit the following events:

```
device.version.update_available  # new version detected for a device class
device.version.updating          # device is applying an update
device.version.updated           # device successfully updated (old->new version)
device.version.update_failed     # device update failed (includes reason)
device.version.rollback          # device rolled back to previous version
device.version.incompatible      # device version is incompatible with controller
```

### 10. Protocol Version Negotiation

When a device connects to the mesh, the first exchange MUST be protocol
version. This is defined in spec 01 (mDNS `proto` TXT field) and spec 09
(`EVT_HELLO`). The routing graph MUST use this to:

1. Set `protocol_version` on the device.
2. Determine which format types and transport plugins the device supports.
3. Filter capability enumeration to the intersection of controller and device
   protocol versions.
4. Surface incompatibility warnings if the device is too old or too new.

Within a major version, the controller MUST speak to the device at the
device's protocol level. A controller at `ozma/0.5` MUST be able to route
through a node at `ozma/0.3` -- it simply does not use features added in
0.4/0.5 on that path. This is transparent to the router: the device's
capability enumeration already reflects what it can do, so the router
MUST NOT select unsupported features.
