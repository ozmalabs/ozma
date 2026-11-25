# Phase 4 — live audio routing (`/audio/*` over PipeWire)

Status: **live-validated** on the host's existing PipeWire/PulseAudio server
(2026-11-25). Two real defects were found and fixed. Sink provisioning is still
owned by `ozma-softnode --provision-audio`, not by `/audio/apply`.

This report covers the real `pactl` + `pw-play` path for the controller
compatibility routes. The UAC2 gadget bridge is a separate path; see
[`phase4-uac2-node-report.md`](phase4-uac2-node-report.md). The routing model
behind these routes is described in [`routing/audio.md`](routing/audio.md).

## Environment

- Host: `SFFPC`, Linux, same-user session.
- `pactl 17.0-134-g77d25`, libpulse 17.0.0.
- `pipewire 1.6.11` / `libpipewire 1.6.11`; server string
  `PulseAudio (on PipeWire 1.6.11)`, server version `15.0.0`, native socket
  `/run/user/1000/pulse/native`.
- Tools present: `pw-play`, `paplay`, `parec`, `pw-record`, `speaker-test`.
- Node: `target/debug/ozma-node` built from the working tree (uncommitted
  changes in this task only).

Node invocation (HTTP fixture port 60371, HID 51943):

```text
ozma-node --profile vm --name audio-live-test \
  --http-port 60371 --hid-udp-port 51943 \
  --rtsp-port 0 --webtransport-port 0 \
  --capture-provider fake --fake-width 320 --fake-height 240 --fake-fps 5 \
  --input-provider fake-recording \
  --audio-router pipewire \
  --audio-track-root /tmp/opencode/audio-live/tracks \
  --api-token <redacted>
```

The reference track was generated locally (`tone.wav`, 4 s, 48 kHz stereo S16,
440 Hz sine at 0.5 full scale), never a microphone recording.

## Graph snapshot

`GET /api/v1/connection` reported `audio: "running"`, `audio_error: null`.

Default devices were unchanged before and after:

| Item | Before | After |
|---|---|---|
| Default sink | `alsa_output.usb-Roland_Rubix22-00.analog-stereo.99` | same |
| Default source | `alsa_input.pci-0000_13_00.6.analog-stereo.99` | same |

Initial sinks (5): the two ALSA outputs above plus HDMI, `ozma-vm1`, `ozma-vm2`
(pre-existing from the demo, left untouched). Initial sources (6): the three
hardware monitors plus the mic, `ozma-vm1.monitor`, `ozma-vm2.monitor`.
Initial modules included the two pre-existing null sinks:

```text
536870916  module-null-sink  sink_name=ozma-vm2
536870917  module-null-sink  sink_name=ozma-vm1
```

For the route test a disposable sink was created with the same shape the
soft-node provisioner uses:

```text
pactl load-module module-null-sink \
  sink_name=ozma-audio-live-1059195 \
  sink_properties=device.description=Ozma-audio-live-test \
  rate=48000 channels=2
# -> module 536870918; sink 42437, monitor source 42437
```

`/audio/apply` does **not** create a sink (see remaining gaps); it applies
volume/mute/route to an existing one. The `AudioRouter::ensure_sink` create /
reuse / remove path was validated separately against the same server (below).

## Route results

All control routes were sent with `Authorization: Bearer <token>`.

| Operation | Request | HTTP | Observed graph effect |
|---|---|---|---|
| `POST /audio/apply` | `{"profile_id":"live-p1","sink":"ozma-audio-live-1059195","volume":37}` | 200 | `pactl get-sink-volume` → `24248 / 37% / -25.91 dB` both channels |
| `POST /audio/apply` | same sink, `volume:55`, `muted:true` | 200 | volume `36044 / 55%`, `Mute: yes` |
| `POST /audio/remove` | `{"profile_id":"live-p2"}` | 200 | volume back to `100%`, `Mute: no` (documented default restore) |
| `POST /audio/apply` | `{"profile_id":"live-src","source":"ozma-audio-live-1059195.monitor","volume":31,"muted":true}` | 200 | `set-source-volume`/`set-source-mute`; source `20316 / 31%`, muted |
| `POST /audio/remove` | `{"profile_id":"live-src"}` | 200 | source back to `100%`, unmuted |
| `POST /audio/apply` | `{"profile_id":"live-final","sink":...,"volume":42,"muted":false}` | 200 | `27525 / 42% / -22.61 dB`, unmuted |
| `POST /audio/play` | `{"track":"tone.wav","sink":"ozma-audio-live-1059195"}` | 200 | `pw-play --target ozma-audio-live-1059195 .../tone.wav`; sink-input `42572 → 42437`; sink state `RUNNING` |
| `POST /audio/stop` | empty body | 200 | no `pw-play`; sink-input list back to the original Chrome stream only |
| `POST /audio/remove` | `{}` (empty body) | 200 for remembered `live-final` | volume back to `100%`, `Mute: no` |

`GET /audio/nodes` and `POST /audio/sweep` are public. They were typed 501 in
this routing pass; the follow-up now returns a real bounded `pactl -f json`
graph snapshot and a measured generated-sweep profile, or a typed 503/502/400.
See [`phase4-audio-sweep-report.md`](phase4-audio-sweep-report.md) for the
graph/sweep implementation, live null-sink measurement, and remaining gaps
(no EQ fitting or DSP). Without `--audio-router` both routes return typed 503s
and still fabricate nothing.

### Captured audio proof

Capture used the disposable sink monitor, not a microphone:

```text
parec --device=ozma-audio-live-1059195.monitor \
      --format=s16le --rate=48000 --channels=2 --raw
```

| Sink volume | Samples | Non-zero samples | Peak | RMS |
|---|---|---|---|---|
| 42% | 65,536 frames (131,072 samples) | 81,782 | 1,214 (`0.037 FS`) | 678.5 (`0.0207 FS`) |
| 100% | 65,536 frames (131,072 samples) | — | 16,383 (`0.500 FS`) | 9,158.7 (`0.2795 FS`) |

The generated file is a 0.5 full-scale sine. At 100% the captured peak is
exactly `0.500 FS`, which proves the tone reached the null sink and its monitor.
At 42% the peak is `0.037 FS`; PulseAudio's cubic volume mapping gives
`0.42³ ≈ 0.074`, and `0.5 × 0.074 ≈ 0.037`, so the capture also proves the
chosen volume was applied.

### `ensure_sink` / `remove_sink` live test

New opt-in test `ozma-audio/tests/router_live.rs` loads a uniquely named
`module-null-sink`, asserts the first `ensure_sink` reports `created: true` and
the `<sink>.monitor` target, the second reports `created: false`, `pactl`
lists the sink, and `remove_sink` unloads it (second remove is a no-op):

```text
OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored
# test result: ok. 1 passed
```

## Controller proxy pass-through

The controller's audio proxy is `_proxy_to_audio_node` in `controller/api.py`.
A FastAPI `TestClient` app was built with a `NodeInfo` pointing at the live node
and the node's per-registration bearer token, then:

```text
POST /api/v1/audio/room-correction/play
{"node_id": "...", "track": "tone.wav", "sink": "ozma-audio-live-1059195"}
-> 200 {"ok": true, "track": "tone.wav", "sink": "...", "backend": "pipewire"}
POST /api/v1/audio/room-correction/stop
{"node_id": "..."}
-> 200 {"ok": true, "backend": "pipewire"}
```

A `parec` capture during the proxied play returned peak `16,383 (0.500 FS)`,
RMS `3,548.7 (0.1083 FS)` (the window includes pre-play silence and the stop),
and `pgrep pw-play` was empty afterwards. The bearer token was sent by the
per-node token resolution; the node also rejects an unauthenticated control
request with 401.

## Error paths

| Case | Result |
|---|---|
| No bearer token on `/audio/apply` | 401 `unauthorized` |
| `volume: 150` | 400 `audio-invalid-volume` (`outside 0..=100`) |
| Missing `profile_id` | 400 `audio-missing-profile` |
| No `sink`, no `--audio-sink` default | 400 `audio-missing-sink` |
| `apply` on a non-existent sink | 502 `audio.command_failed` (`pactl ... No such entity`) |
| Missing track file | 400 `audio.track_not_found` with resolved path |
| `track: "../etc/passwd"` | 400 `audio.invalid_spec` (single-file-name rule) |
| Empty `track` | 400 `audio-missing-track` |
| `play` with no sink and no default | 400 `audio-missing-sink` |
| `play` with a non-existent sink (after fix) | 400 `audio.refused`, no player spawned |
| `remove` with an unknown profile | 200 (documented idempotent no-op) |
| `stop` with nothing playing | 200 (documented no-op) |
| Node without `--audio-router` | all four control routes 501 `audio-routing-unsupported`; connection `audio: "not-configured"` |
| `--audio-router pipewire` with unreachable `PULSE_SERVER` | connection `audio: "failed"` with `audio_error`; control routes 503 `audio-routing-unavailable` |

## Bugs found and fixed

### 1. `/audio/play` claimed success for a missing sink and played on the default sink

Before the fix, `POST /audio/play` with
`{"track":"tone.wav","sink":"ozma-audio-live-missing"}` returned **200** and
spawned:

```text
pw-play --target ozma-audio-live-missing /tmp/opencode/audio-live/tracks/tone.wav
```

`pw-play` ignored the unknown target and created sink-input `42538` on the
**default** sink (the host's USB Roland output). The route had no way to know
the sink was missing, so it reported success while audio went somewhere else.
The stray stream was stopped immediately through `POST /audio/stop` (no
permanent change), and the pre-fix graph was restored.

Fix: `RouterCore::play_reference` now checks `pactl list short sinks` after
validating the track and before spawning. An absent sink is a typed
`AudioError::Refused` → HTTP 400 `audio.refused`, with remediation to create
the sink first. This is covered by
`router::tests::missing_playback_sink_is_refused_and_does_not_spawn`
(a fake runner with an empty sink list asserts no spawn).

Post-fix live result:

```text
POST /audio/play {"track":"tone.wav","sink":"ozma-audio-live-missing"}
-> 400 {"code":"audio.refused","details":"playback sink `...` does not exist", ...}
pgrep pw-play -> empty
```

### 2. Startup probe repeated `exit status` and embedded newlines

A node with `--audio-router pipewire` and an unreachable server reported:

```text
audio_error: pipewire audio server is unavailable:
  `pactl info` failed with exit status exit status: 1: Connection failure: ...
```

`ExitStatus` already displays as `exit status: 1`, and the original code
prepended `exit status` again. The multi-line stderr was also embedded raw in
JSON.

Fix: `probe_failure_detail` uses the status display once and collapses stderr
to one line. Post-fix:

```text
audio_error: pipewire audio server is unavailable:
  `pactl info` failed with exit status: 1: Connection failure: Connection refused
```

Covered by `main::tests::probe_failure_detail_does_not_repeat_the_status_prefix`.

## Reproduced checks

```text
cargo test -p ozma-audio -p ozma-node --lib
# ozma-audio: 99 passed, 1 ignored
# ozma-node: 635 passed, 4 ignored

cargo test -p ozma-node --lib public_
# 6 passed

cargo test -p ozma-node --bin ozma-node probe_failure_detail
# 1 passed

cargo check -p ozma-audio -p ozma-node --all-targets
# Finished

OZMA_AUDIO_LIVE=1 cargo test -p ozma-audio --test router_live -- --ignored --nocapture
# 3 passed (live bounded graph; disposable null sink create/reuse/remove; measured sweep)

python3 -m pytest tests/unit/controller/test_node_api_token_proxy.py -q
# 87 passed
```

`rustfmt --edition 2021` was run on every touched Rust file.

## Cleanup

- Unloaded the disposable module: `pactl unload-module 536870918`.
- `pactl list short sinks|sources|modules` no longer contains
  `ozma-audio-live-1059195` or its `.monitor`.
- Sink, source, module and sink-input lists are identical before/after; the
  only original sink-input (`42338 → 20614`, Chrome) is still the only one.
- Default sink/source are unchanged.
- Both test nodes were killed; `pgrep -a pw-play` and `pgrep -a parec` are
  empty.
- Pre-existing `ozma-vm1`/`ozma-vm2` sinks were reused only as snapshot
  context and were never modified or unloaded.

## Remaining gaps

1. **No sink creation through `/audio/apply`.** The route applies a profile to
   an existing sink (or the `--audio-sink` default). Sink creation remains
   `AudioRouter::ensure_sink` / `ozma-softnode --provision-audio`
   (`<prefix><vm>`, default `ozma-<vm>`). The live route test therefore
   pre-created the null sink with `pactl`.
2. **Graph enumeration and sweep are read-only/basic.** `GET /audio/nodes` now
   lists sinks, sources, sink-inputs, and source-outputs from `pactl -f json`
   with bounded counts; `pactl` exposes no graph links, so `links` stays empty
   and `links_available` is false. `POST /audio/sweep` generates and plays a
   log sweep, captures the named source, and reports peak/RMS/noise/impulse and
   a best-effort RT60. It does **not** fit or apply EQ bands, and RT60 is `null`
   when the captured decay does not span enough range. See
   [`phase4-audio-sweep-report.md`](phase4-audio-sweep-report.md).
3. **No route for source creation or stream moves.** `ensure_source`,
   `remove_source`, `move_stream` and `move_source_output` exist on the router
   and are unit-tested with fakes, but the controller surface only exposes
   volume/mute for an existing sink/source.
4. **Controller apply/remove proxy is thin.** The room-correction
   apply/remove proxies forward only `profile_id` (plus `node_id`), not
   sink/source/volume/mute, and map a node HTTP error to
   `{"ok": false, "status": N}` without the node's typed body. They rely on the
   node's `--audio-sink` default.
5. **Play success is still return-on-spawn.** The player is checked for a
   missing sink and track, but a player that exits immediately after spawning
   (for example a corrupt file with a valid name) can still report 200 until
   `stop`/reap. Adding a bounded liveness check would need a `try_wait` on the
   `CommandRunner` child.
6. **No DSP / room correction / multi-channel layouts.** Volume, mute and
   routing only; no EQ, convolution, channel maps or latency compensation.
7. **Single-host, same-user validation.** Remote PulseAudio servers,
   authentication, and non-Linux hosts were not exercised.
