# Phase 4 — active room-correction EQ insertion

Status: **implemented and live-validated** on the host's existing PipeWire
1.6.11 / pipewire-pulse 15.0.0 server (2026-11-26).

`POST /audio/room-correction/apply` now loads a reversible
`libpipewire-module-filter-chain` for a measured profile, and `DELETE
/audio/room-correction/apply` removes it. The implementation targets one
explicit sink that the caller names, never changes the default sink/source, and
never moves an existing stream. This document records the insertion mechanism,
the rejected alternatives, the API contract, and the evidence.

## 1. What the host can actually load

The host runs PipeWire, WirePlumber, and pipewire-pulse:

```text
pactl 17.0-134-g77d25 (libpulse 17.0.0)
pipewire 1.6.11 (libpipewire 1.6.11)
wireplumber, pipewire, pipewire-pulse: active
```

Three candidate paths were checked.

### 1.1 `pactl load-module module-filter-chain`

```text
$ pactl load-module module-filter-chain 'filter.graph = { ... }'
Failure: No such entity
```

`module-filter-chain` is a native PipeWire module, not a PulseAudio module.
The `pipewire-pulse` server implements a fixed subset of PulseAudio module names
(`module-null-sink`, `module-remap-source`, `module-loopback`,
`module-native-protocol-tcp`, ...). Its supported-name list does not include
`module-filter-chain`, and there is no `module-equalizer-sink`
(`pactl load-module module-equalizer-sink` also returns `No such entity`).
There is **no runtime module-load API** on the PipeWire daemon, so this path is
a hard blocker on this host, not a configuration mistake.

### 1.2 Restarting or editing daemon config

Adding a fragment under `~/.config/pipewire/pipewire.conf.d/` would require
restarting the user audio graph. That would disrupt every existing stream and
is explicitly out of scope. Rejected.

### 1.3 A short-lived `pipewire -c <generated config>` client

PipeWire supports running an independent client context that loads
`libpipewire-module-filter-chain` and publishes its nodes into the existing
graph. This is the mechanism the shipped `filter-chain.service` uses. It does
not restart the daemon, and killing the client removes its nodes.

Validated manually:

```text
# generated config: one bq_peaking band, smart target ozma-eqtest-sink
$ pipewire -c /tmp/.../filter-chain.conf &
$ pactl list short sinks
43818  ozma-eqtest-sink   PipeWire  float32le 2ch 48000Hz  SUSPENDED
43824  ozma-eq-test       PipeWire  float32le 2ch 48000Hz  SUSPENDED
$ pw-link -l
ozma-eqtest-sink:playback_FL
  |<- output.filter-chain-2165202-7:output_FL
ozma-eqtest-sink:playback_FR
  |<- output.filter-chain-2165202-7:output_FR
$ kill <pipewire pid>
# both the filter node and its output node disappear
```

The generated filter uses `capture.props.media.class = Audio/Sink`,
`filter.smart = true`, `filter.smart.name`, and an explicit
`filter.smart.target = { node.name = "<target>" }`, so WirePlumber links the
filter output into the named target. `playback.props.node.passive = true` and
`media.role = "DSP"` keep the target suspended when no client is playing.

**Chosen path: 1.3.** The EQ graph is a real PipeWire module, but it lives in a
child client that Ozma owns and can kill. No daemon restart, config drop-in, or
default-device change is involved.

## 2. Insertion shape

`ozma-audio/src/eq.rs` validates and renders the config; `RouterCore` owns the
process.

For each apply:

1. Validate profile id, target sink, EQ sink name, 1–10 bands, gains within
   ±12 dB, Q within 0.1–10, frequency within the sweep bounds, rate 8–192 kHz,
   and 1–2 channels.
2. Check that the target sink exists through `pactl list short sinks`.
3. Refuse if the generated EQ sink name already exists. An Ozma apply never
   adopts or replaces an unrelated sink.
4. Unload any previous Ozma chain (explicit replacement, never two chains).
5. Write a private config directory (`0700`) and config file (`0600`) under the
   process temp directory, with unique pid/nanosecond/sequence names.
6. Spawn `pipewire -c <config>` through the existing argv-only
   `CommandRunner` (no shell, no interpolation).
7. Poll `pactl list short sinks` for the new EQ sink for up to 5 s. On timeout
   or any probe error, kill the child and return a typed error.
8. Only after the sink is observed, record the in-process application and
   return it.

The generated graph is a serial biquad chain:

```text
low_shelf  -> bq_lowshelf   (Freq, Q, Gain)
peaking    -> bq_peaking    (Freq, Q, Gain)
high_shelf -> bq_highshelf  (Freq, Q, Gain)
```

Example fragment:

```text
filter.graph = {
    nodes = [
        { type = builtin name = band0 label = bq_lowshelf control = { "Freq" = 63.000 "Q" = 1.000 "Gain" = 3.000 } }
        { type = builtin name = band1 label = bq_peaking  control = { "Freq" = 1000.000 "Q" = 1.400 "Gain" = -2.500 } }
    ]
    links = [
        { output = "band0:Out" input = "band1:In" }
    ]
}
capture.props = {
    node.name           = "ozma-eq-sweep-test-1"
    media.class         = Audio/Sink
    filter.smart        = true
    filter.smart.name   = "ozma-eq-sweep-test-1"
    filter.smart.target = { node.name = "ozma-vm1" }
}
playback.props = {
    node.passive = true
    media.role   = "DSP"
}
```

### What "apply to a sink" means here

The filter publishes a **new EQ sink** and links its output to the named target
sink. Audio is corrected when it is played to the EQ sink. Ozma does not move
existing streams onto it and does not change the default sink, because either
action would mutate the user's graph implicitly. The response reports both
names so a caller can route explicitly:

```json
{
  "ok": true,
  "active": true,
  "target_sink": "ozma-vm1",
  "eq_sink": "ozma-eq-sweep-test-1",
  "module_id": 1,
  "module_name": "libpipewire-module-filter-chain",
  "active_bands": 10,
  "band_count": 10,
  "insertion": "pipewire-filter-chain-smart-sink"
}
```

`module_id` is the Ozma runtime handle for the filter-chain child (the
`CommandRunner` child id), not a `pactl` module id; `module_name` names the
actual PipeWire module. This is deliberate: pipewire-pulse never creates a
module id for this path.

## 3. API

### `POST /audio/room-correction/apply`

Body (all fields optional except as noted):

| Field | Meaning |
|---|---|
| `profile` | Full `SweepProfile` object. When omitted, the stored profile is used. |
| `profile_id` | Selects/checks the stored profile; a mismatch with the inline or stored profile is rejected. |
| `sink` / `target_sink` | Explicit target sink; falls back to the node's `--audio-sink`. |
| `rate` | Filter rate, default 48 kHz, 8–192 kHz. |
| `channels` | 1 or 2, default 2. |

The profile is validated with the same `validate_profile` used by storage, so
an out-of-range gain, an `active:true` measurement, a band-count mismatch, or a
profile without captured audio is rejected before any process is spawned.

Responses:

| Case | Result |
|---|---|
| No `--audio-profile-dir` or no router | 501 `audio-eq-application-unsupported` |
| Invalid bands/rate/channels/profile id | 400 `audio.invalid_spec` |
| Missing target sink or EQ sink collision | 400 `audio.refused` |
| No stored profile and no inline profile | 404 `audio-profile-missing` |
| Filter did not publish the EQ sink in 5 s | 503 `audio.process_failed` |
| `pipewire` binary missing | 503 `audio.missing_command` |
| PulseAudio-only router | 501 `audio.unsupported` |

### `DELETE /audio/room-correction/apply`

Takes no body. Unloads and reaps the child, deletes the config directory, and
clears the persisted active record. Returns `removed:true` with the former
profile/sink ids, or `removed:false` when nothing was active. A second delete is
a successful no-op.

### Controller proxy

- `POST /api/v1/audio/room-correction/apply` forwards `profile_id`, an optional
  `profile`, and optional `sink`/`rate`/`channels` to the node.
- `POST /api/v1/audio/room-correction/remove` proxies the node `DELETE
  /audio/room-correction/apply`.

Typed node bodies and statuses pass through unchanged; a transport failure is
the existing typed 502 `audio.proxy_failed`.

## 4. Active-state persistence and teardown

The measured profile stays `active:false`; it is a measurement, not a claim.
The runtime state is a separate file in the same opt-in directory:

```text
room-correction-profile.json
room-correction-eq-active.json
```

`room-correction-eq-active.json` is written atomically (`0600`, temp file +
`fsync` + rename), capped at 64 KiB, and validated on read. It is written only
after the EQ sink was observed. If the write fails, the route tears the module
down before returning the error.

Teardown paths:

- **Explicit delete:** kill child, drop config directory, clear state file.
- **Replacement apply:** unload the old child before loading the new one.
- **Startup:** the node clears any stale active record; a fresh process cannot
  own an older child, and a disconnected PipeWire client removes its nodes.
- **Node shutdown:** the child is spawned with `kill_on_drop(true)`, so the
  filter chain is killed when the router/process exits. The config directory is
  removed by its RAII guard.

The default sink and default source are never set, moved, muted, or unloaded.
Only the disposable target sink named by the caller is referenced.

## 5. Tests and live evidence

### Unit and route tests

`cargo test -p ozma-audio --lib`:

- `eq::tests::*` — config graph, smart target, bounds, private temp file,
  application JSON round trip, sink naming.
- `router::tests::apply_room_eq_spawns_pipewire_and_removes_the_child` — spawn,
  poll, report, remove, config deletion.
- `router::tests::apply_room_eq_refuses_missing_targets_and_sink_collisions_without_spawning`.
- `router::tests::pulseaudio_does_not_claim_filter_chain_support`.
- `room::tests::active_state_round_trips_and_is_separate_from_the_profile`.
- `room::tests::corrupt_or_unsafe_active_state_is_rejected`.

`cargo test -p ozma-node --lib` route tests:

- stored-profile apply → state file → delete → second delete;
- router failure is 503 and leaves no active record;
- missing profile is 404, invalid gain is 400, mismatched id is 400.

Controller pytest:

- `test_audio_room_eq_apply_proxies_to_the_node_and_preserves_typed_errors`
- `test_audio_room_eq_remove_proxies_delete_and_returns_the_node_body`

### Live run

```text
$ OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test eq_live -- --ignored --nocapture
running 2 tests
test filter_chain_refuses_a_missing_target_without_leaking_a_sink ... ok
live EQ: sink ozma-eq-live-profile-2 -> target ozma-eq-live-<pid>,
  module 1 (3 bands), default sink alsa_output.usb-Roland_Rubix22-00.analog-stereo.99
test filter_chain_applies_to_a_disposable_sink_and_removes_cleanly ... ok
```

The live test:

- created a disposable `module-null-sink`;
- loaded a 3-band chain (low shelf, peaking, high shelf) targeting it;
- asserted the EQ sink appeared in `pactl list short sinks`;
- asserted `pw-link -l` linked `output.filter-chain-*` into the target sink;
- asserted the default sink string was unchanged before and after;
- removed the chain and asserted the EQ sink disappeared while the target
  remained;
- asserted a missing target is refused without leaking a sink;
- removed the target sink at the end.

Post-run checks: no `ozma-eq-*` sink, no filter-chain module, and no
`pipewire -c .../ozma-eq-*` process remained.

## 6. Remaining gaps

1. **Not transparent.** The caller must route audio to `eq_sink`; no stream is
   moved and no default sink is changed. This is the deliberate safety boundary.
2. **1–2 channels only.** Multichannel targets need a channel-position aware
   graph and are not claimed.
3. **WirePlumber smart-filter dependency.** On a host without WirePlumber smart
   filters, the module may load without the output link. The current health
   check proves the EQ sink exists; a future check could also prove the link.
4. **One active chain per node.** Applying a new profile unloads the previous
   chain rather than running multiple zones.
5. **No live microphone validation.** The loopback proves insertion/removal and
   graph isolation, not audible correction in a real room.
6. **Runtime handle, not a module id.** `module_id` is Ozma's child handle
   because pipewire-pulse exposes no module id for this path.
