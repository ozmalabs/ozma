# External Switches

**Status:** Draft

## RFC 2119 Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

External switches — KVM switches, HDMI matrix switches, audio matrix switches,
and crosspoint switches — are devices with multiple input ports and one or more
output ports whose internal routing is configurable. This document specifies how
the routing protocol models switch controllability, tracks routing matrix state
with appropriate confidence levels, and governs router behaviour when
incorporating switches into pipelines.

## Specification

### Controllability

Not all switches are equal. Some can be read and written, some can only be
commanded with no feedback, and some are entirely manual (the user presses a
physical button). Every switch device MUST declare its controllability so that
the router can make correct decisions:

```yaml
Controllability:
  state_readable: bool          # can we query current routing state?
  state_writable: bool          # can we command routing changes?
  feedback: FeedbackModel       # what kind of confirmation do we get?
  control_interface: string?    # how we talk to it ("serial", "ir", "ip", "usb", "cec", "manual")
  control_plugin: string?       # plugin id that handles this device

FeedbackModel: enum
  confirmed       # device reports current state (readable + writable)
  write_only      # we can command it but never confirm (IR blaster, many serial devices)
  manual          # no electronic control — user operates it physically
  event_only      # device emits state changes but cannot be commanded
```

### Switch Routing Matrix

A switch has a configurable internal routing matrix — which input ports are
connected to which output ports. This MUST be modelled as switchable internal
links:

```yaml
SwitchMatrix:
  device_id: string
  routes: SwitchRoute[]
  matrix_type: one_to_one | many_to_one | many_to_many

SwitchRoute:
  input_port: PortRef           # input port on the switch
  output_port: PortRef          # output port on the switch
  active: bool                  # is this route currently active?
  state_quality: InfoQuality    # how confident are we in this state?
```

For a `write_only` device, `state_quality` MUST be `commanded` — we sent the
switch command but have no confirmation it was applied. The router MUST account
for this uncertainty.

For a `confirmed` device, `state_quality` MUST be `reported` — the device told
us its current state.

For a `manual` device, `state_quality` SHOULD be `assumed` after the user tells
us what they set, or `user` if they explicitly confirmed it.

### Router Behaviour with Switches

1. **Confirmed switches**: The router MUST treat switchable internal links like
   any other link. It MAY activate routes, read state, and trust the response.

2. **Write-only switches**: The router MUST send the switch command and mark the
   internal link as active with `commanded` quality. If the pipeline then fails
   (e.g., no video arriving when expected), the router SHOULD retry or alert the
   user, since the switch state is uncertain.

3. **Manual switches**: The router MUST NOT attempt to activate routes — it
   SHOULD only recommend the required action. The pipeline MUST be marked as
   requiring user action. Once the user confirms the switch position, the route
   SHOULD be marked `user` quality.

4. **Event-only switches**: The router MUST observe state changes (e.g., a KVM
   switch with hotkey detection that reports which input is active) but MUST NOT
   command changes. These devices MAY be used for integrating existing
   infrastructure the user controls manually but Ozma should be aware of.

The router MUST check control path reachability before attempting to switch a
writable device. If the control interface (serial, IP, IR) is unavailable, the
router MUST NOT assume the switch will succeed.

### Examples

| Device | Controllability | Typical interface |
|--------|----------------|-------------------|
| TESmart HDMI matrix | confirmed | Serial (RS-232) or IP |
| Cheap HDMI switch with IR | write_only | IR blaster |
| HDMI CEC-capable TV | confirmed | CEC over HDMI |
| Manual desktop KVM | manual | Physical button |
| Enterprise Extron matrix | confirmed | Serial or IP, full status feedback |
| AV receiver (HDMI inputs) | confirmed or write_only | IP, serial, or CEC |

### Switch as Bridge

A switch in the graph acts as a bridge between otherwise disconnected segments.
If Machine A's HDMI output goes through an HDMI matrix to a capture card on
Machine B, the routing graph shows:

```
Machine A: GPU:hdmi-out → hdmi-cable → Matrix:input-3
Matrix: input-3 → [switchable internal link] → output-1
Matrix: output-1 → hdmi-cable → Capture Card:hdmi-in
Capture Card: hdmi-in → internal → Capture Card:usb-out → ...
```

The router MAY activate `input-3 → output-1` on the matrix (if writable) as
part of assembling the pipeline. The switch command MUST be part of pipeline
activation, not a separate operation.
