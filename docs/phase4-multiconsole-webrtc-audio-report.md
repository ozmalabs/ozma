# Phase 4 — per-console WebRTC audio sources

Follow-up to `docs/phase3-live-console-switch-report.md` §15. The
`--multi-console-webrtc` manager used to give every per-console helper the same
base `--webrtc-audio-source`, so two simultaneous console helpers could only
monitor one shared sink. This change adds an optional per-console template and
reports each helper's audio configuration honestly. It does not add audio
sample capture, negotiation, or live claims.

## Configuration

`--multi-console-webrtc-audio-source TEMPLATE`
(`OZMA_MULTI_CONSOLE_WEBRTC_AUDIO_SOURCE`) sets a template for helpers started
by the per-console manager:

```text
--multi-console-webrtc \
--webrtc-helper /usr/bin/python3 \
--webrtc-helper-script scripts/webrtc/gst_webrtc_worker.py \
--multi-console-webrtc-audio-source 'ozma-vm{console}.monitor'
```

- `{console}` is replaced with the advertised console's zero-based index.
- `{index}` is an accepted synonym (`{console}` and `{index}` may both appear).
- `ozma-vm{console}.monitor` yields `ozma-vm0.monitor` for `Console_0` and
  `ozma-vm1.monitor` for `Console_1`.
- A template without placeholders is valid and gives every helper the same
  source; this is only useful when the operator really wants a shared monitor.

The expansion is applied when the manager builds each helper's `WebRtcConfig`,
before `WebRtcHelper::start`, so the helper argv receives only that console's
`--audio-source`. The shared base `--webrtc-audio-source` is ignored when the
template is set.

When the template is unset, behavior is unchanged:

- a base `--webrtc-audio-source` is passed to every helper (the historical
  shared source), or
- without a base source every helper stays video-only.

A console whose factory expansion has no source is also video-only; the
manager does not invent or borrow another console's monitor.

## Validation

`--multi-console-webrtc-audio-source` is validated during node configuration
and again when a console's helper config is built:

- empty or whitespace-only templates are rejected;
- control characters are rejected;
- the template is bounded to 256 characters and the expanded source to 512;
- an expansion that becomes empty is rejected;
- unknown placeholders (for example `{consol}`) are rejected instead of being
  passed through silently;
- the console count is already bounded by `MAX_VM_CONSOLES` and the manager's
  `--multi-console-webrtc-max` helper bound.

Errors are typed configuration errors (`--multi-console-webrtc-audio-source:
...`) at startup. A helper whose expansion fails at start time is recorded as
`failed` with the bounded error rather than started with a partial argv.

## Status

`/api/v1/connection.multi_console_webrtc.consoles[]` now includes:

- `audio_configured`: true when that console's helper was configured with a
  monitor;
- `audio_source`: the expanded monitor name, omitted when video-only.

Monitor names are not secrets, so they are reported as configured. These fields
are configuration intent only. They do not claim the worker opened the monitor,
that an audio m-line was negotiated, or that any samples arrived. The
compatibility-level `webrtc_audio_configured` is true when either the shared
base source or a per-console template is configured.

## Tests

`cargo test -p ozma-node --lib` covers:

- `{console}`/`{index}` expansion and the shared-source/no-placeholder case;
- empty, whitespace, control-character, over-long, and unknown-placeholder
  template rejection;
- `ControllerWebRtcFactory::config_for` passing the expanded source into
  `WebRtcConfig`, including a fake helper `helper_argv` assertion for
  `--audio-source ozma-vm1.monitor`;
- unset-template preservation of a shared base source and the video-only
  fallback;
- per-console manager status with audio on one console and video-only on the
  other;
- `/api/v1/connection` serialization of per-console `audio_configured` /
  `audio_source` without a samples field;
- CLI parsing of the template flag and `NodeConfig::validate` rejection.

No GStreamer process, PipeWire/PulseAudio monitor, or capture device is opened
by these tests. No audio samples are produced or claimed.

Verification on 2026-11-26: `cargo test -p ozma-node --lib` passed 778 tests
(7 ignored), and `cargo check -p ozma-node --all-targets` passed. The focused
`multi_webrtc::tests` run passed all 13 tests. The template adds no helper or
session collection: helper count remains bounded by
`--multi-console-webrtc-max`, session routing by the existing
`MAX_SESSIONS_PER_CONSOLE`, and expanded sources by 512 characters.

## Remaining WebRTC gaps

- Audio remains entirely dependent on the external helper/Jupyter worker
  pipeline; there is no native Rust `webrtc-rs`/`str0m` audio path.
- Per-console audio is still one monitor per helper. There is no mixer,
  per-guest sink discovery, or automatic `<sink>.monitor` derivation from VM
  names; operators must supply the template or base source.
- The status does not prove the helper opened the monitor, negotiated an audio
  m-line, or delivered samples. A live per-console audio run (two monitors,
  two peers) has not been performed here.
- `/codec` still applies only to HLS; WebRTC keeps its existing encoder.
- Single-head providers (QMP/VNC/Looking Glass) remain typed `unsupported` for
  simultaneous per-console helpers.
