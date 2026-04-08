# Relationship to Existing Protocols

**Status**: Draft
**RFC 2119 Conformance**: The key words "MUST", "MUST NOT", "REQUIRED", "SHALL",
"SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

---

## Abstract

This document describes how the Ozma routing specification relates to existing
protocol specifications and external protocols modelled in the routing graph.
The routing specification is a layer above these protocols: it provides the
model that determines when and how they are used, without replacing them.

This document is **informational and non-normative**. No conformance requirements
are placed on implementations by this document. The tables below use "maps to"
language to describe relationships, not obligations.

---

## Specification

### 1. Relationship to Ozma Protocol Specifications

The routing protocol operates as a coordination layer above the individual
protocol specifications. Each existing spec maps to a routing concept as follows:

| Existing spec | Relationship to routing |
|--------------|------------------------|
| 01 -- Discovery (mDNS) | Discovery layer 3: populates graph with network-visible nodes |
| 02 -- HID Transport | Transport plugin: maps to `udp-direct` or `udp-aead` carrying HID format |
| 03 -- Audio: VBAN | Transport plugin: maps to `vban` carrying uncompressed audio format |
| 04 -- Audio: Opus RTP | Transport plugin: maps to `rtp-opus` carrying compressed audio format |
| 05 -- Video: MJPEG/UVC | Transport plugin: maps to `udp-mjpeg` carrying MJPEG video format |
| 06 -- Video: H.265 Sunshine | Transport plugin: maps to `sunshine` carrying H.265 video format |
| 07 -- Control Plane | Maps to the API through which routing is observed and controlled |
| 08 -- OTA | Maps to device versioning and update delivery (see versioning.md) |
| 09 -- Event/Command | Maps to control plane transport for node commands including pipeline activation |
| 10 -- Presence/Display | Device plugin: presence nodes map to sensor/data sources; display nodes map to screen sinks |
| 11 -- Peripheral RGB | Transport + format: maps to `rgb` media type with DDP/Art-Net/E1.31/vendor transports |

### 2. Additional Protocols Modelled in the Routing Graph

The following protocols are not part of the Ozma specification series but are
modelled as device plugins, transport plugins, or both within the routing graph:

| Protocol/System | Relationship to routing |
|----------------|------------------------|
| WebRTC | Transport plugin: maps to browser-based video/audio/HID with DTLS |
| Looking Glass (IVSHMEM) | Transport plugin: maps to zero-copy VM display via shared memory |
| QMP / libvirt | Device plugin: maps to VM host discovery, HID injection, power control |
| RTSP / ONVIF / NDI | Device + transport plugins: maps to camera discovery, video source, PTZ control |
| Bluetooth (A2DP/HFP/BLE) | Transport plugin: maps to audio and control over Bluetooth |
| MQTT | Transport plugin: maps to IoT sensor/actuator data, doorbell events |
| KDE Connect | Transport + device plugin: maps to phone as compound device |
| OSC | Transport plugin: maps to network control surface input/feedback |
| MIDI | Transport plugin: maps to control surface with bidirectional feedback |
| DDC/CI | Transport plugin: maps to monitor brightness, power, input switching |
| CEC | Transport plugin: maps to HDMI device control, switch commands |
| Serial (RS-232) | Transport plugin: maps to switch/actuator control, serial consoles |
| NUT | Transport + device plugin: maps to UPS monitoring |
| WoL | Transport plugin: maps to Wake-on-LAN magic packets to target devices |
| DDP / Art-Net / E1.31 | Transport plugins: maps to network RGB protocols |
| PipeWire | Transport plugin: maps to same-machine audio linking |

### 3. Role of the Routing Protocol

The routing protocol's role is to decide: for a given intent, which of these
protocols to activate, with what parameters, between which endpoints. The
individual protocols remain the wire-level implementation.
