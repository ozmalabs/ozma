# Intents

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD",
"SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this document are to be
interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

An intent declares an outcome the user or system wants. It is a first-class
object in Ozma, not a pipeline recipe: selecting a menu item expresses a desired
outcome, and the graph resolves that intent against its current state. This
document specifies intent requests and profiles, interpretation and resolution,
the built-in structured profiles, composition, degradation policies, and
extension mechanisms.

## Specification

### 3.1 Intent structure

An intent request MUST preserve the desired outcome independently of any
particular route, device, provider, codec, or transport selected to fulfil it.
The intent remains stable while the graph may produce a different resolution
as devices, links, features, services, measurements, or policy change.

```yaml
IntentRequest:
  id: string                    # stable request identity
  expression: IntentExpression # structured, free_text, or an extension kind
  requirements: Requirement[]?  # optional hard requirements
  preferences: Preference[]?   # optional ranking hints
  context: ContextRef[]?        # optional user, application, target, or scene context
  requested_by: ParticipantRef

IntentExpression:
  structured: StructuredIntent? # typed, machine-actionable expression
  free_text: string?            # natural-language expression for an interpreter
  extension: ExtensionValue?    # namespaced, discoverable expression type
```

`IntentExpression` has exactly one active form. A request MAY omit explicit
requirements and preferences: the resolver may infer defaults from the intent,
context, policy, and graph. If requirements are supplied, they constrain
resolution; they do not prescribe the implementation route. For example,
"put application X on screen Y" names an outcome. It does not require the user
to select a codec, transport, capture node, or pipeline.

Structured intents can be evaluated directly by a capable node or service.
Free-text intents require an interpreter service that can convert the
expression into a structured intent or a set of candidate outcomes. An
interpreter is an ordinary graph participant with advertised features; it may
be remote and need not share the user's local network. The graph routes the
request to an eligible interpreter or arbitrator based on reachability,
capability, policy, cost, and availability.

Intent interpretation, resolution, and execution are distinct stages:

1. **Interpretation** derives a structured request from the expression when
   needed. It may ask for clarification or return multiple candidates.
2. **Resolution** evaluates the request against graph state and returns a
   proposal: outcomes, participating features and devices, unmet requirements,
   alternatives, and an explanation. Resolution MAY be distributed across
   interpreter, arbitrator, and domain services.
3. **Execution** realizes an accepted resolution. It may assemble one or more
   pipelines and invoke services or devices. The user intent remains the source
   of truth; execution details are derived artefacts.

Implementations MUST make the request and its resolution distinguishable in
state and events. A graph change MAY invalidate a resolution and trigger a new
one without changing the user's intent. Whether a revised resolution is
automatically executed or requires renewed approval depends on policy and the
intent's authorization requirements.

The protocol substrate MUST provide stable, language-neutral framing and
negotiation for exchanging intent requests, resolution proposals, decisions,
and lifecycle events. Protobuf is the canonical schema and encoding for this
core exchange. Intent kinds, feature identifiers, and extension payloads are
open and namespaced; adding or removing an intent type MUST NOT require adding
a closed enum value to the substrate. HTTP/JSON, browser, and compatibility
bindings MAY expose equivalent representations. Media payloads continue to use
transports suited to their timing and bandwidth requirements.

### 3.2 Structured intent profile

```yaml
Intent:
  name: string                  # identifier (built-in or user-defined)
  description: string           # human-readable purpose
  streams: StreamIntent[]?      # optional per-media-type requirements
  priority: uint?               # relative priority when resources are contended
  degradation: DegradationPolicy? # optional response when requirements cannot be met
```

**StreamIntent**:

```yaml
StreamIntent:
  media_type: video | audio | hid | screen | rgb | control | data
  required: bool                # MUST this stream exist for the intent to be satisfied?
  constraints: Constraints      # hard limits -- pipeline MUST be rejected if these can't be met
  preferences: Preferences      # soft targets -- optimise toward these, accept less
```

**Constraints** (hard -- the router MUST reject a pipeline if any constraint is violated):

```yaml
Constraints:
  max_latency_ms: float?        # end-to-end latency ceiling (steady-state)
  max_activation_time_ms: float? # maximum time to go from current state to active (see S2.6)
  min_bandwidth_bps: uint64?    # minimum available bandwidth required
  max_loss: float?              # maximum acceptable loss rate
  max_jitter_ms: float?         # maximum acceptable jitter
  max_hops: uint?               # maximum number of links traversed
  max_conversions: uint?        # maximum format changes allowed
  required_formats: Format[]?   # MUST support at least one of these formats
  forbidden_formats: Format[]?  # MUST NOT use these formats
  encryption: required | preferred | none  # data-in-transit encryption requirement
```

**Preferences** (soft -- used for ranking candidate pipelines):

```yaml
Preferences:
  target_latency_ms: float?     # ideal latency
  target_resolution: Resolution? # ideal video resolution
  target_framerate: float?      # ideal framerate
  target_sample_rate: uint?     # ideal audio sample rate
  target_channels: uint?        # ideal audio channel count
  target_bit_depth: uint?       # ideal audio bit depth
  prefer_lossless: bool?        # prefer lossless codecs when available
  prefer_hardware_codec: bool?  # prefer hardware encode/decode
  prefer_fewer_hops: bool?      # weight hop count more heavily
  prefer_lower_latency: bool?   # weight latency more heavily
  prefer_higher_quality: bool?  # weight quality more heavily
```

### 3.3 Built-in structured profiles

The following standard intents MUST be available in every Ozma implementation.
Users MAY modify their parameters or define entirely new intents.

#### `control`

Headless operation. Keyboard and mouse only, no video or audio needed.

```yaml
name: control
description: Keyboard and mouse input to a headless or blind target
priority: 90
streams:
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
      max_loss: 0.001
    preferences:
      prefer_lower_latency: true
  - media_type: video
    required: false
  - media_type: audio
    required: false
```

#### `preview`

Low-resolution, high-latency video for thumbnails and machine pickers. Cheap to
produce and transport.

```yaml
name: preview
description: Low-res thumbnail for dashboards, machine picker, monitoring grid
priority: 10
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 2000
    preferences:
      target_resolution: { w: 640, h: 360 }
      target_framerate: 2
      prefer_lower_latency: false
      prefer_higher_quality: false
  - media_type: hid
    required: false
  - media_type: audio
    required: false
```

#### `observe`

High-resolution monitoring where latency is secondary. Security cameras, kiosk
checks, passive monitoring.

```yaml
name: observe
description: High-quality passive monitoring -- latency is acceptable
priority: 20
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 1000
    preferences:
      target_resolution: { w: 1920, h: 1080 }
      target_framerate: 15
      prefer_higher_quality: true
  - media_type: audio
    required: false
    constraints:
      max_latency_ms: 1000
    preferences:
      target_channels: 1
      prefer_lossless: false
  - media_type: hid
    required: false
```

#### `desktop`

General productivity. Good video, reasonable latency, stereo audio. The default
for most KVM usage.

```yaml
name: desktop
description: General productivity -- good video, reasonable latency, stereo audio
priority: 50
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 100
    preferences:
      target_resolution: { w: 1920, h: 1080 }
      target_framerate: 30
      prefer_lower_latency: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 50
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      target_bit_depth: 16
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 10
      max_loss: 0.0001
```

#### `creative`

Video/audio production, CAD, music. High resolution, low latency, multichannel
lossless audio.

```yaml
name: creative
description: Content creation -- high resolution, low latency, lossless audio
priority: 70
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 50
    preferences:
      target_resolution: { w: 3840, h: 2160 }
      target_framerate: 60
      prefer_higher_quality: true
      prefer_hardware_codec: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 20
      forbidden_formats:
        - { codec: opus }
        - { codec: aac }
        - { codec: mp3 }
    preferences:
      target_channels: 8
      target_sample_rate: 96000
      target_bit_depth: 24
      prefer_lossless: true
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
```

#### `gaming`

Interactive gaming. Native resolution, minimum latency for both video and input.
Gamepad support.

```yaml
name: gaming
description: Interactive gaming -- minimum latency, native resolution, gamepad
priority: 80
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 16
    preferences:
      target_framerate: 60
      prefer_lower_latency: true
      prefer_hardware_codec: true
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 20
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      prefer_lower_latency: true
  - media_type: hid
    required: true
    constraints:
      max_latency_ms: 5
      max_loss: 0.0001
    preferences:
      prefer_lower_latency: true
degradation:
  video: reduce_framerate_first
  audio: allow_lossy_compression
  hid: never_degrade
```

#### `broadcast`

One-to-many distribution. Source quality, latency is irrelevant. Teacher mode,
screen share, recording source.

```yaml
name: broadcast
description: One-to-many distribution -- source quality, latency irrelevant
priority: 30
streams:
  - media_type: video
    required: true
    constraints:
      max_latency_ms: 5000
      max_conversions: 1
    preferences:
      prefer_higher_quality: true
      prefer_fewer_hops: true
  - media_type: audio
    required: false
    constraints:
      max_latency_ms: 5000
    preferences:
      target_channels: 2
      prefer_higher_quality: true
  - media_type: hid
    required: false
```

#### `fidelity_audio`

Audio is the primary concern. No compression, ever. Video and HID are
irrelevant. Music listening, mixing, mastering, room correction measurement.

```yaml
name: fidelity_audio
description: Uncompromised audio -- never compressed, multichannel, high sample rate
priority: 60
streams:
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 10
      forbidden_formats:
        - { codec: opus }
        - { codec: aac }
        - { codec: mp3 }
        - { codec: vorbis }
        - { codec: wma }
        - { lossy: true }
    preferences:
      target_channels: 8
      target_sample_rate: 96000
      target_bit_depth: 32
      prefer_lossless: true
  - media_type: video
    required: false
  - media_type: hid
    required: false
```

These profiles are reusable structured expressions or defaults, not a closed
list of outcomes the substrate can carry. Implementations and plugins MAY
register additional namespaced intent kinds without changing core negotiation.

### 3.4 Intent composition

Intents are composable. A user MAY want `control` + `fidelity_audio` (typing on
a headless audio workstation). When intents are composed:

1. Each intent's stream requirements MUST be merged per media type.
2. Constraints MUST be intersected -- the strictest constraint wins.
3. Preferences MUST be merged; conflicts MUST be resolved by the higher-priority intent.
4. Priority MUST be the maximum of the composed intents.

Composition produces a new synthetic intent that the router MUST treat
identically to a named intent.

### 3.5 Degradation policy

When the graph cannot satisfy an intent's constraints, the degradation policy
SHOULD be applied to determine what happens:

```yaml
DegradationPolicy:
  # per-media-type degradation strategy
  video: DegradationStrategy
  audio: DegradationStrategy
  hid: DegradationStrategy

DegradationStrategy: enum
  never_degrade       # fail the pipeline rather than degrade this stream
  reduce_framerate_first   # video: drop framerate before resolution
  reduce_resolution_first  # video: drop resolution before framerate
  allow_lossy_compression  # audio: permit lossy codec as last resort
  reduce_sample_rate       # audio: lower sample rate before allowing lossy
  increase_latency         # accept higher latency to maintain quality
  drop_stream              # remove this stream entirely rather than degrade others
```

The router MUST apply degradation strategies in order of the intent's stream
priorities. HID SHOULD almost never be degraded -- it is tiny bandwidth and
critical for usability. Audio next. Video last (it has the most room to degrade
gracefully).

### 3.6 Custom intents

Users MAY define custom intents in the same YAML schema. Custom intents MAY
extend or override built-in intents:

```yaml
name: my_streaming_setup
extends: broadcast
description: My Twitch streaming with specific audio requirements
streams:
  - media_type: audio
    required: true
    constraints:
      max_latency_ms: 100
    preferences:
      target_channels: 2
      target_sample_rate: 48000
      target_bit_depth: 24
      prefer_lossless: true
```

Fields in the child override the parent. Streams MUST be merged by `media_type` --
a child stream definition replaces the parent's definition for that media type.
