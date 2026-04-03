# Network Architecture

Ozma can run in two networking modes: **appliance mode** (alongside an existing
router) and **router mode** (as the primary router/gateway). In both modes it
manages an encrypted mesh for KVM traffic, a dedicated IoT/camera VLAN, and an
optional built-in 2.4 GHz AP for device onboarding.

This document covers the network features beyond the KVM mesh. For the KVM mesh
itself see [Security Architecture](security.md) and `controller/mesh_network.py`.

---

## Why this exists

IoT security is solved in theory and broken in practice. Everyone knows cameras,
thermostats, and smart bulbs should be on an isolated VLAN that can't reach the
rest of the LAN or the internet unless explicitly allowed. Almost nobody does this,
because the steps involved — VLAN config, firewall rules, AP SSID isolation,
per-device exceptions during onboarding, post-onboarding lockdown — require
networking knowledge that most people don't have and time that nobody wants to spend.

The result is that most home and small-office networks have smart devices sitting
on the main LAN with full access to every PC, NAS, and phone. These devices
run unaudited firmware, phone home to servers in jurisdictions with no privacy
laws, and in some cases are actively hostile (see: Flock, Ring, and the broader
history of "smart" device data sharing with governments and advertisers).

Ozma fixes this without requiring the user to understand VLANs. The complexity
is in the software. The user sees a workflow.

---

## Architecture overview

```
Internet
    │
    ▼
[WAN / existing router]
    │
    │  LAN (192.168.x.x — user's existing network)
    ├──────────────────────────────────────┐
    │                                      │
    ▼                                      ▼
[Ozma Controller]                    [PCs, phones, NAS]
    │  manages:
    ├─ Ozma KVM mesh (WireGuard, 10.200.x.x)
    │
    ├─ IoT VLAN (172.16.0.0/24)
    │     ├─ cameras (Frigate, local only)
    │     ├─ smart bulbs, plugs, sensors
    │     ├─ thermostats
    │     └─ firewall: deny → main LAN, allow → Ozma controller only
    │
    └─ 2.4 GHz AP (built-in or USB dongle)
          └─ IoT SSID → IoT VLAN only
             Main SSID → bridges to existing router (passthrough)
```

In **router mode**, the controller sits between WAN and LAN and owns all of this
natively. In **appliance mode**, it integrates with existing network equipment to
create the VLAN and firewall rules via API (UniFi, MikroTik, OpenWrt, pfSense/
OPNsense). In both modes the user-facing workflow is identical.

---

## IoT VLAN

### Default policy

All IoT VLAN traffic is governed by a default-deny ruleset:

| Direction | Default | Override |
|-----------|---------|----------|
| IoT → Internet | **Deny** | Per-device allow rules (e.g. thermostat → manufacturer API) |
| IoT → Main LAN | **Deny** | Never opened (unless explicitly requested) |
| IoT → Ozma Controller | **Allow** | Required for Frigate, HA integration, monitoring |
| Main LAN → IoT | **Deny** | Opened temporarily during device setup only |
| Internet → IoT | **Deny** | Permanent |

This means: by default, an IoT device can do nothing except talk to the Ozma
controller. It cannot phone home. It cannot reach the user's PCs. It cannot be
reached from the internet.

### Per-device cloud allow rules

Some devices legitimately need cloud connectivity (thermostats with scheduling
APIs, voice assistant bridges, etc.). These are opt-in:

```json
{
  "device_id": "nest-thermostat-1",
  "allow_outbound": [
    { "host": "home.nest.com", "port": 443, "protocol": "tcp" }
  ]
}
```

The controller generates nftables/iptables rules from this. Unknown destinations
remain blocked. The user can review and revoke cloud permissions at any time from
the dashboard.

### Camera devices

Camera devices get an additional constraint: **no internet access by default,
ever**. Cameras are the highest-risk IoT category (see Ring, Flock, Eufy).
If a camera is added and the user requests cloud access, the dashboard shows a
warning explaining the implications and requires explicit confirmation.

Frigate connects to cameras on the IoT VLAN directly from the controller. The
video never leaves the local network unless the user explicitly configures
recording upload or remote viewing through Ozma's encrypted relay.

---

## Device onboarding workflow

The problem with IoT VLANs is onboarding: the device needs to be provisioned
(given a WiFi password, account linked, etc.) from a phone or PC, which means
the phone needs to be on the same network as the device temporarily. After
onboarding, that temporary access must close. Nobody does this correctly because
it requires manually toggling firewall rules around the setup process.

Ozma automates the entire sequence.

### Easy mode (recommended)

1. User opens the dashboard or Ozma mobile app and taps **Add IoT device**.
2. Selects device type (camera, thermostat, smart plug, etc.) and optionally
   the manufacturer.
3. Ozma creates a **setup exception**: the user's phone (identified by its current
   IP or MAC) gets temporary access to the IoT VLAN.
4. The phone connects to the IoT SSID or is temporarily bridged into the IoT VLAN.
5. User completes the device's normal setup workflow (Wyze app, Nest app, etc.) —
   Ozma does not interfere with this step.
6. User taps **Done** in the Ozma dashboard.
7. Ozma removes the setup exception, applies the default-deny policy to the new
   device's MAC/IP, and adds the device to the inventory.
8. From this point, the device is isolated. The dashboard shows it as **secured**.

The user never touches a VLAN configuration. They never write a firewall rule.
They just follow the normal device setup process while Ozma manages the network
around it.

### Advanced mode

For power users who want to inspect or customise the generated rules:

- Full nftables rule preview before applying
- Per-device allow/deny rule editor
- DHCP reservation (fixed IP per device)
- Custom DNS entries (e.g. redirect cloud hostnames to local alternatives)
- Port forwarding from mesh to IoT device (e.g. expose a local API)

---

## Built-in 2.4 GHz AP

Many IoT devices only support 2.4 GHz WiFi. Rather than requiring the user to
have a router that supports IoT VLAN isolation (most consumer routers do not),
the controller can run its own 2.4 GHz AP dedicated to the IoT VLAN.

### Hardware options

| Option | Hardware | Notes |
|--------|----------|-------|
| First-party device | Embedded MediaTek MT7603/MT7612 or similar | On-die; no USB required |
| DIY | USB WiFi dongle (MT7601U, RTL8188, Atheros AR9271) | ~$5 on AliExpress; AP mode supported on all |
| High-end DIY | USB WiFi 6 adapter (MT7921AU) | 2.4+5 GHz, better range |

The AP runs **hostapd** on a bridge interface bound to the IoT VLAN. The controller
manages hostapd config and restarts automatically when IoT VLAN policy changes.

SSIDs:
- **`Ozma-IoT`** (or user-configured name): bridges to IoT VLAN only
- **`Ozma-Setup`** (hidden, temporary): used during device onboarding, disabled after

The main LAN SSID is not served by the Ozma AP — the user's existing router/AP
handles that. Ozma's AP is purely for IoT and onboarding.

### No existing hardware required

This is the key design point: a $5 USB dongle eliminates the dependency on the
user's router supporting VLANs. Any Linux box with a USB port can run this.

---

## Integration with existing homelab hardware

For users who already have managed network equipment, Ozma drives it via API
instead of managing the network natively. The result is the same: IoT VLAN,
firewall rules, AP isolation.

### UniFi

- **API**: UniFi Controller REST API (local, no cloud)
- **Capabilities**: VLAN creation, network isolation, AP SSID-to-VLAN binding,
  firewall group rules, client VLAN override per MAC
- **What Ozma does**: creates `ozma-iot` network, configures firewall rule groups,
  sets SSID VLAN assignment, manages per-device firewall entries
- **Config**: `OZMA_UNIFI_URL`, `OZMA_UNIFI_USER`, `OZMA_UNIFI_PASS` in env

### MikroTik (RouterOS)

- **API**: RouterOS REST API (v7+) or legacy API socket
- **Capabilities**: VLAN interfaces, bridge port isolation, address lists,
  firewall filter rules
- **What Ozma does**: adds VLAN interface, adds firewall chain for IoT, manages
  address list for per-device rules
- **Config**: `OZMA_MIKROTIK_URL`, API credentials

### OpenWrt

- **API**: ubus JSON-RPC over HTTP, UCI commit
- **Capabilities**: VLAN (DSA or legacy swconfig), firewall zones, AP SSID isolation
- **What Ozma does**: UCI batch config for network/firewall/wireless, applies via
  ubus or SSH as fallback
- **Config**: `OZMA_OPENWRT_URL`, credentials

### pfSense / OPNsense

- **API**: pfSense REST API or OPNsense `fauxapi` / native REST
- **Capabilities**: VLANs, interface groups, firewall rules, DHCP server
- **What Ozma does**: creates VLAN interface, firewall rules via API,
  DHCP static mappings for enrolled devices
- **Config**: `OZMA_PFSENSE_URL` or `OZMA_OPNSENSE_URL`, API key

### No supported hardware (DIY / bare Linux)

Ozma falls back to managing the network directly via Linux primitives:
- VLAN: `ip link add link eth0 name eth0.100 type vlan id 100`
- Bridge: `ip link add name br-iot type bridge`
- Firewall: nftables rules written to `/etc/nftables.d/ozma-iot.conf`
- DHCP: dnsmasq instance dedicated to the IoT interface
- AP: hostapd on the dedicated WiFi interface

This mode requires the controller to be in the network path (router mode) or
connected to a managed switch with VLAN trunking.

---

## Router mode

When the controller is the primary gateway, it handles routing and NAT in
addition to the KVM mesh and IoT VLAN:

```
WAN (DHCP or static) ──▶ [Ozma Controller] ──▶ LAN (192.168.1.0/24)
                                   │
                                   ├──▶ IoT VLAN (172.16.0.0/24)
                                   ├──▶ Ozma KVM mesh (10.200.0.0/16)
                                   └──▶ Optional: DMZ, guest, VoIP VLANs
```

- NAT/masquerade via nftables
- DHCP server (dnsmasq) per subnet
- DNS resolver with split-horizon (IoT devices can be given fake local DNS)
- Optional: IDS/IPS via Suricata or Snort (on first-party hardware with enough
  CPU/NPU)
- Optional: ad/tracker blocking (Pi-hole / AdGuard Home) for IoT VLAN by default

Router mode is the target mode for first-party Ozma hardware. It is also fully
functional on any Linux box with two network interfaces.

---

## Camera hardware

See [Camera Recommendations](cameras.md) for detailed guidance on camera and
PoE switch selection. The short version: **use wired PoE cameras only**. Wireless
cameras are jammable, side-channel leakable, and unreliable. The recommended open
stack (Reolink PoE + TP-Link PoE switch + Frigate) costs roughly one-third of an
equivalent UniFi Protect setup with no vendor lock-in and no cloud dependency.

---

## Frigate integration

When the controller has sufficient resources (first-party hardware, or user's
existing machine), Frigate NVR runs locally:

- Frigate connects to cameras on the IoT VLAN over RTSP
- Object detection runs on local hardware (Intel NPU on N100, GPU, or CPU)
- Recordings stored locally; no cloud unless user explicitly configures upload
- Ozma dashboard embeds Frigate UI and clips
- Frigate events fire Ozma notifications (Slack, Discord, webhook, Ozma mobile)
- Frigate is the only process that can reach cameras — everything else is blocked
  by the IoT firewall rules

On hardware without sufficient resources, Frigate is not started. The camera feed
is still captured and available via Ozma (RTSP proxy through the IoT firewall) but
object detection is not available without cloud assist (opt-in).

Frigate resource requirements:
- Minimum: 4-core CPU, 4 GB RAM (software detection, limited cameras)
- Recommended: Intel N100 / N305 with QSV + NPU, 8 GB RAM (10+ cameras)
- GPU: NVIDIA with CUDA, AMD with ROCm (for large deployments)

---

## Privacy implications

This architecture directly addresses the surveillance network problem:

- **No camera data leaves the network by default.** Cameras on the IoT VLAN
  cannot reach the internet. Their streams are accessible only to Frigate on the
  controller and to authenticated Ozma remote sessions.
- **No device can phone home by default.** The default-deny policy means a
  compromised or malicious IoT device cannot exfiltrate data. It is network-isolated
  at the packet level.
- **Onboarding is the only window.** During the setup exception, the device can
  reach the phone for provisioning. The exception closes automatically when setup
  completes.
- **Cloud allow rules are explicit and auditable.** If a device is given internet
  access, the user approved it and it is recorded in the audit log. The dashboard
  shows exactly which devices have outbound internet access and to where.
- **Ozma does not see camera content.** Video is RTSP from camera to Frigate, local
  only. It does not transit Connect. The relay architecture (WireGuard L3 forwarder)
  means even if remote viewing is enabled, Connect sees only encrypted packets.

See [Privacy Architecture](privacy.md) for the full data handling commitments.

---

## Implementation modules

| Module | Purpose |
|--------|---------|
| `controller/iot_network.py` | IoT VLAN lifecycle, device inventory, firewall rule generation |
| `controller/ap_manager.py` | hostapd management, IoT SSID, setup SSID lifecycle |
| `controller/network_router.py` | Router mode: NAT, DHCP, DNS, nftables ruleset |
| `controller/net_integrations/unifi.py` | UniFi Controller API client |
| `controller/net_integrations/mikrotik.py` | MikroTik RouterOS API client |
| `controller/net_integrations/openwrt.py` | OpenWrt ubus/UCI client |
| `controller/net_integrations/pfsense.py` | pfSense/OPNsense REST client |
| `ecosystem/frigate-tools/` | Frigate integration (existing) |

These modules extend `controller/mesh_network.py` which already handles the KVM
mesh, USB Ethernet gadget, and distributed firewall for node traffic.
