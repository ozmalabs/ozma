# Camera Recommendations

**TL;DR: Use wired PoE cameras. Never wireless.**

This document explains why, and gives specific hardware recommendations for
building an open, local, privacy-respecting camera system that is easier to
operate than Ubiquiti UniFi Protect and costs a fraction of the price.

---

## Why not wireless cameras

Wireless cameras are not a security product. They are a convenience product that
looks like a security product. The failure modes are well-documented:

### Jamming

RF jammers that block 2.4 GHz and 5 GHz WiFi are illegal to operate in most
jurisdictions and are sold openly on AliExpress for $20–40. A burglar with a
$20 jammer can disable every wireless camera on your property before entering.
This is not a theoretical attack. It is used in real burglaries and is well
documented by law enforcement.

Wired cameras cannot be jammed. There is no radio frequency to interfere with.

### Side-channel leakage

Encrypted wireless camera traffic still leaks information through traffic
analysis. Even when you cannot read the content, the pattern of packets reveals:

- **Motion events**: a camera sends a burst of data when it detects motion.
  An observer monitoring your WiFi can tell when motion was detected without
  decrypting a single packet.
- **Occupancy**: patterns of camera activity across the day reveal when your
  property is occupied or empty.
- **Camera locations**: WiFi signal strength and direction from multiple
  observation points can be triangulated to find where cameras are mounted.
- **Number of cameras**: the number of active streams is visible from traffic
  patterns.

This is published academic research, not speculation. See "Peek-a-Boo: I see
your smart home activities" (2020) and subsequent work on IoT traffic analysis.

Wired cameras on an isolated VLAN produce no observable RF signal. There is
nothing to intercept.

### Reliability

Wireless cameras depend on WiFi availability. An AP reboot, interference from
a neighbour's router, or a microwave oven can cause dropped frames or complete
outages. In a security context, a gap in coverage is exactly what an attacker
waits for.

Wired cameras work as long as the cable and switch have power. PoE switches
can be backed by UPS for continuous operation during power outages.

### Physical substitution

A wireless camera can be removed from its mount and replaced with an identical
device running different firmware, without the attacker ever touching the network.
The next camera to join the SSID looks identical to the original.

A wired camera requires physical access to the cable run. The MAC address is
visible in switch ARP tables and can be monitored for unexpected changes.

---

## Why PoE specifically

Power over Ethernet delivers both power and data over a single Cat5e/Cat6 cable.
This matters because:

- **One cable per camera**: no mains power socket required at the camera location.
  This makes installation dramatically simpler and enables locations that would
  otherwise be impractical.
- **Centralised power management**: the PoE switch is the single power distribution
  point. Cameras can be rebooted remotely by toggling the switch port. UPS coverage
  requires only one device — the switch — rather than one per camera.
- **Tamper detection**: loss of link on a switch port is an event. A camera being
  physically removed is immediately visible in the switch port state and can fire
  an alert.

**Standard**: IEEE 802.3af (PoE, 15.4W per port) covers all cameras listed below.
802.3at (PoE+, 30W) is required only for PTZ cameras with integrated heaters.

---

## Recommended hardware

### Cameras

All recommendations require: RTSP stream support, local storage or NVR-only mode,
no mandatory cloud account, wired PoE.

#### Doorbell replacement (direct Ring alternative)

**Reolink Video Doorbell PoE** (~$60–80)

A wired PoE doorbell camera that replaces Ring with no cloud, no subscription,
and no data sharing. Single Cat6 cable carries power and data. Works with Frigate
for person detection and clip storage. Two-way audio supported.

- 5MP, 180° vertical FOV (designed for doorstep packages and faces)
- Doorbell button → Frigate event → Ozma push notification to phone
- Two-way audio via RTSP back-channel
- RTSP: `rtsp://<ip>:554/h264Preview_01_main`
- PoE: 802.3af (requires adapter if replacing an existing wired doorbell — the
  existing low-voltage doorbell wiring carries only 16–24VAC, not PoE; run Cat6
  or use a PoE injector at the door)
- Approximate cost: $60–80

This is the recommended starting point for a Ring replacement. One camera, one
cable, one push notification when someone is at the door. From the end user's
perspective it is indistinguishable from Ring — except the footage stays on their
hardware and is never shared with anyone.

---

#### Budget tier (~$30–60 per camera)

**Reolink RLC-810A (8MP / 4K)**

The best budget PoE camera. RTSP support is explicit and well-documented. Works
with Frigate out of the box. No cloud account required. Local NVR mode supported.

- 4K / 8MP sensor, good night vision (colour + IR)
- RTSP: `rtsp://<ip>:554/h264Preview_01_main`
- PoE: 802.3af
- Approximate cost: $35–50

**Reolink RLC-510A (5MP)**

Slightly lower resolution, same excellent integration story. Use where 4K
bandwidth is a concern.

- 5MP sensor
- RTSP: same path format as above
- PoE: 802.3af
- Approximate cost: $30–40

#### Mid-range tier (~$60–120 per camera)

**Dahua IPC-HDW2849H-S-IL (8MP)**

Dahua OEM hardware underlies many popular brands (Amcrest, Lorex, others). The
Dahua native firmware has full RTSP, ONVIF, and local NVR support. Better low-light
performance than Reolink at this tier.

- 4K / 8MP, Smart Dual Light (colour night vision)
- RTSP: `rtsp://<ip>:554/cam/realmonitor?channel=1&subtype=0`
- PoE: 802.3af
- Approximate cost: $60–80

Note: Hikvision and Dahua are Chinese state-connected manufacturers. If this
is a concern for your threat model, use Reolink (Taiwanese) or step up to Axis.

**Amcrest IP8M-2496EW** (Dahua OEM, sold without Dahua branding)

Same hardware as above with Western support and warranty. Good choice if you
prefer not to buy from Dahua directly.

#### High-end tier (~$150–400 per camera)

**Axis P3245-V / M3106**

Axis (Swedish) cameras are the standard for professional installations. Excellent
image quality, long-term firmware support, ACAP application platform for edge
processing, ONVIF profile S/G. No cloud dependency.

- Better optics and sensor quality than budget tiers
- Local analytics available (motion zones, tripwires, object classification)
- PoE: 802.3af / 802.3at depending on model
- Approximate cost: $150–250

Axis cameras integrate directly with Frigate and ONVIF. If you are installing in
a commercial or high-value environment, Axis is the right choice.

---

### PoE switches

#### Small installs (1–4 cameras)

**TP-Link TL-SG1005P** (~$30)

5-port, 4× PoE (802.3af, 41W total budget). Unmanaged. Sufficient for 1–3
cameras and the controller on one switch. Plug-and-play, no configuration.

**TP-Link TL-SG1008P** (~$45)

8-port, 4× PoE (802.3af, 55W total). Good for 4 cameras + other devices.

#### Medium installs (4–12 cameras)

**TP-Link TL-SG108PE** (~$60)

8-port, 4× PoE+ (802.3at, 64W total). Managed — supports VLAN tagging, port
monitoring, and cable diagnostics. Recommended if you want per-port VLAN
assignment (so cameras are isolated to the IoT VLAN at the switch level, not
just by firewall rules).

**TP-Link TL-SG1218MP** (~$120)

16-port, 12× PoE+ (802.3at, 250W total). Good for larger installs. Managed,
supports 802.1Q VLANs. This is the Ozma-managed IoT VLAN sweet spot: each
camera port is tagged to the IoT VLAN in the switch, the uplink port carries
the trunk to the controller.

#### Large installs (12+ cameras)

**TP-Link TL-SG3428XMP** (~$300)

28-port, 24× PoE+ with 10G uplinks. L2+ managed. For large deployments where
camera count justifies the hardware.

At this scale, UniFi switches are also a reasonable choice — Ozma integrates
with the UniFi Controller API to manage VLANs and firewall rules. The cameras
themselves should still be RTSP-native (not UniFi Protect cameras, which require
the UniFi Protect NVR ecosystem).

---

## The full open stack

```
Internet (blocked for cameras)
    │
[Ozma Controller]
    │ IoT VLAN only
    │
[PoE switch] ── VLAN trunk ──────────────────────┐
    │                                             │
    ├──[PoE port 1]── Cat6 ──[Reolink cam 1]     │
    ├──[PoE port 2]── Cat6 ──[Reolink cam 2]     │
    ├──[PoE port 3]── Cat6 ──[Reolink cam 3]     │
    └──[PoE port N]── Cat6 ──[Reolink cam N]     │
                                                  │
                              [Frigate NVR] ◄─────┘
                              running on Ozma controller
                              RTSP pull from each camera
                              object detection (local NPU/GPU)
                              recordings stored locally
                              events → Ozma notifications
```

Cost for a 4-camera install:
- 4× Reolink RLC-810A: ~$160
- 1× TP-Link TL-SG1005P: ~$30
- Cat6 cable + connectors: ~$20
- **Total: ~$210**

Equivalent UniFi Protect setup (4× G4 Bullet + UniFi NVR): **~$700–900**,
requires cloud account, locked to UniFi hardware, camera footage handled by
Ubiquiti's servers if cloud backup is enabled.

---

## Comparison to UniFi Protect

| | Open stack (Ozma + Frigate) | UniFi Protect |
|---|---|---|
| Camera cost (4K, 4 cameras) | ~$160 (Reolink) | ~$500 (G4 Bullet × 4) |
| NVR / recorder | Frigate on existing hardware | UniFi NVR ($180+) or Cloud Key |
| Cloud account required | No | Required for some features |
| RTSP access | Yes, always | Restricted by default |
| Vendor lock-in | None | Cameras only work with UniFi Protect |
| Object detection | Frigate (local, fast) | Unifi AI (cloud-assisted) |
| Object detection privacy | Local only, no data leaves | Frames processed by Ubiquiti |
| Integration with HA / Frigate | Native | Workarounds required |
| Self-hosted | Yes | Partial (NVR must be Ubiquiti) |
| Firmware source | Closed, but RTSP always exposed | Closed, RTSP gated |

UniFi Protect is a polished product. Its UX is excellent. The reason not to use it
is: vendor lock-in, cost, and the architecture decision to route camera intelligence
through Ubiquiti's cloud rather than processing it locally. For a system where
camera footage is sensitive, local processing is not optional.

---

## Camera placement recommendations

These are general recommendations for residential and small commercial installs:

- **Entrances first**: front door, back door, garage entrance. These are the
  highest-value positions.
- **Cover approach paths**: driveways, side gates, paths that lead to entry points.
  Cameras should trigger before an intruder reaches a door.
- **Height and angle**: mount at 2.5–3m for best facial coverage without easy
  reach. Angle slightly downward. Avoid mounting so high that you only see the
  top of heads.
- **Avoid aiming at public spaces**: cameras should cover your property, not
  streets or neighbours' property. Check local regulations.
- **Lighting matters more than IR**: colour cameras with good existing lighting
  outperform IR cameras in total darkness for identification purposes. Consider
  adding motion-triggered lighting alongside cameras.
- **Weatherproofing**: IP67 minimum for external cameras. All recommended cameras
  above meet this.

---

## Ring replacement — consumer setup guide

Ring succeeds for one reason: the person who uses the camera does not need to
understand how it works. Unbox, scan QR code, app works. That is the bar.

Ozma matches this for the end user. The difference is that someone technically
capable does the initial setup — and that person might not be the person who
lives with the camera. **This is the correct model for recommending Ozma to
non-technical friends and family.**

### The gifting setup

Scenario: you want to give someone a privacy-respecting doorbell camera as a
gift. They are not technical. They currently have Ring or are considering it.

**What you buy (~$150–180 one-time, no subscription):**

| Item | Cost |
|------|------|
| Reolink Video Doorbell PoE | ~$70 |
| TP-Link TL-SG1005P (PoE switch) | ~$30 |
| Cat6 cable + RJ45 crimp kit | ~$15 |
| N100 mini-PC (controller, if they don't have one) | ~$100–150 |
| *or*: add to your own Ozma controller | ~$0 |

**What you do (one afternoon):**

1. Run Cat6 from the PoE switch to the door. Terminate both ends.
2. Mount the Reolink doorbell. Connect the cable.
3. Add the camera to Frigate on the controller. Configure person detection and
   a motion zone covering the doorstep.
4. Set up Ozma push notifications for person detected / doorbell pressed events.
5. Create a user account for them in the Ozma dashboard with `guest` role (read,
   live view, clips — no admin).
6. Open the Ozma app on their phone. Log in with their account. Show them the
   live view and where clips appear.

**What they do from that point:**

Nothing technical. They get a push notification when someone is at the door.
They tap it to see the live view. They can see recent clips. Two-way audio
works from the app. Exactly like Ring.

**What they don't get:**

- Monthly subscription ($4–10/month for Ring Protect, $50–120/year)
- Their footage uploaded to Amazon
- Their footage shared with law enforcement on request without a warrant
  (Ring fulfilled over 11 million police data requests in 2022 alone)
- A device that stops working if Ring shuts down the service or discontinues
  the product

**Ongoing maintenance:**

Near zero. The controller runs unattended. Frigate updates via the Ozma update
manager. If something breaks, you can fix it remotely via the Ozma relay — they
never need to touch configuration.

### Why Ring sharing matters to the neighbour, not just you

If your neighbour has a Ring camera at their door, their footage — including
footage of you walking past — is accessible to Amazon and to law enforcement
agencies that have signed Amazon's partnership agreements, often without a
warrant. The footage is not just of your neighbour's property. It includes
the public space in front of their home, and everyone who passes through it.

This is not a hypothetical concern. Ring's own transparency reports document
the volume of data requests fulfilled. The ACLU, EFF, and multiple academic
researchers have documented the scope of the programme.

Suggesting Ozma to a neighbour is not just about their privacy. It is about yours.

### Doorbell on your desk

Because Ozma is a KVM routing platform with full audio control, the doorbell
experience on a desk is fundamentally better than Ring — not just equivalent.

**The scenario**: you are at your PC wearing a headset. The doorbell rings.

With Ring: your phone buzzes. You pick it up, unlock it, open the Ring app,
tap Answer. By the time you're talking, 10–15 seconds have passed.

With Ozma: a notification overlay appears on your monitor showing the live
doorbell feed. You hear the chime through your headset. You click Answer.
You talk through the headset you are already wearing. The camera speaker
plays your voice. You never touch your phone.

This works because Ozma already knows:
- Which machine is currently active (the KVM switch state)
- What audio devices are connected to that machine (the headset, via PipeWire)
- How to push an overlay to the active screen (the screen manager)
- How to route audio between sources (VBAN / PipeWire)

Frigate fires the doorbell event. Ozma routes it to wherever you are. If you
switch machines mid-call the audio follows. If you are not at any machine, it
falls back to your phone.

Ring can only go to your phone because Ring does not know where you are or what
audio gear you have. Ozma does.

This generalises beyond doorbells:
- Frigate person alert → thumbnail overlay on active screen, dismiss or pull up full feed
- Package delivery detected → silent notification with clip, no interruption
- Any camera event → routed to wherever you are, using the audio and display you already have

### Feature comparison with Ring

| Feature | Ring | Ozma + Reolink |
|---------|------|----------------|
| Push notification on motion | Phone only | Phone + desktop overlay |
| Push notification on doorbell press | Phone only | Phone + desktop overlay + headset chime |
| Answer doorbell | Phone app | Headset + screen overlay — no phone needed |
| Live view | Phone app | Any screen (phone, desktop, any active machine) |
| Two-way audio | Phone mic/speaker | Headset mic/speaker on active machine |
| Audio follows machine switch | No | Yes |
| Person detection | Cloud | Local (Frigate AI) |
| Motion clips saved | Subscription required | Local, no subscription |
| Activity zones | Yes | Yes (Frigate zones) |
| Night vision | Yes | Yes |
| Works without internet | No | Yes |
| Monthly subscription | $4–10/month | None |
| Footage shared with police | Yes (documented) | No — footage never leaves local network |
| Works if company shuts down | No | Yes |

The one area where Ring leads is app polish. That gap closes. The data sharing
and the desk experience gaps do not.

---

## What to avoid

| Product category | Why |
|-----------------|-----|
| Any wireless camera | Jammable, side-channel leakage, unreliable |
| Ring, Nest, Eufy | No RTSP, cloud-mandatory, data sharing with governments (documented) |
| UniFi Protect cameras | RTSP gated, locked to UniFi Protect ecosystem |
| Cameras without local RTSP | You do not control your footage |
| Cameras requiring a mobile app for setup with no local fallback | App becomes unavailable, camera becomes a brick |
| Cameras on your main LAN | Use the IoT VLAN — see [Network Architecture](network.md) |
