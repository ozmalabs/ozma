# Phase 4 HLS audio report

**Scope:** opt-in AAC audio for the local `ozma-node` HLS pipeline, including
the single active stream and per-console `--multi-console-hls` streams.

**Result: PASS for configured-monitor AAC muxing into HLS.** A disposable
PulseAudio/PipeWire null sink with a generated tone produced HLS MPEG-TS
segments that `ffprobe` reports with both `h264`/`hevc` video and `aac` audio
and plausible durations. Video-only remains the default and the historical
argv is unchanged apart from the bounded HLS muxer queue.

This work deliberately does **not** change the default sink or source, does not
create audio devices, and does not touch `ozma-usb`, `ozma-mesh`,
`ozma-multiseat`, `ozma-softnode`, or the controller Python.

## 1. Configuration

| Flag / env | Meaning |
|---|---|
| `--hls-audio` / `OZMA_HLS_AUDIO` | Opt in to HLS audio. The monitor is derived from `--audio-sink` or a `monitor:<sink>` `--audio-capture-target`. |
| `--hls-audio-source <monitor>` / `OZMA_HLS_AUDIO_SOURCE` | Explicit PulseAudio/PipeWire monitor source, for example `ozma-vm1.monitor`. Implies `--hls-audio`. |
| `--hls-audio-bitrate-kbps <n>` / `OZMA_HLS_AUDIO_BITRATE_KBPS` | AAC bitrate target. Default 128; bounded to 8–512 kbps. |
| `--multi-console-hls-audio-source <template>` / `OZMA_MULTI_CONSOLE_HLS_AUDIO_SOURCE` | Optional per-console monitor template. `{console}` and `{index}` expand to the zero-based console index. |

Resolution rules:

- An explicit `--hls-audio-source` wins.
- Otherwise `--hls-audio` appends `.monitor` to a named `--audio-sink`.
- Otherwise it uses `<sink>.monitor` from a `monitor:<sink>`
  `--audio-capture-target`.
- Otherwise startup fails with a typed error. No default source is invented and
  no silence is synthesized.
- Per-console HLS uses the template when supplied. When it is unset, every
  per-console pipeline keeps the base HLS monitor (or stays video-only when the
  base has no monitor). The template is validated at startup for console `0`
  and the largest possible console index. A base monitor's bitrate wins for
  template-expanded consoles; when there is no base monitor, the template uses
  `--hls-audio-bitrate-kbps`.

`HlsConfig.audio` stores `{ source, bitrate_kbps }`. `HlsConfig::validate`
rejects an empty/whitespace source, control characters, and bitrates outside
the bound. This is configuration intent only; it is not proof that the monitor
exists or produced samples.

## 2. ffmpeg pipeline

The video input is still the existing rawvideo/compressed stdin input. When
audio is configured, ffmpeg gets a real second input and an AAC output:

```text
ffmpeg -hide_banner -loglevel warning \
  -f rawvideo -pix_fmt rgb24 -s WxH -r FPS -i - \
  -f pulse -wallclock 0 -fragment_size 4096 -i <monitor> \
  -map 0:v:0 -map 1:a:0 \
  ...existing H.264/HEVC/AV1 encoder, GOP, color, HLS options... \
  -af aresample=async=1:first_pts=0 -c:a aac -b:a <n>k -ar 48000 -ac 2 \
  -thread_queue_size 512 -f hls ...
```

Details:

- **Second input, not generated audio.** The monitor is opened by ffmpeg's
  PulseAudio input, which also works through PipeWire's pulse compatibility
  server. No `anullsrc` or silent fallback exists.
- **Timestamps.** `-wallclock 0` makes the monitor demuxer start at PTS 0
  instead of the current wall clock, matching the zero-based raw-video
  timeline. `aresample=async=1:first_pts=0` also aligns the first audio samples
  and lets ffmpeg correct drift.
- **Bounded queues.** `-fragment_size 4096` bounds monitor buffering (about
  21 ms at 48 kHz stereo s16). The Rust feeder's asynchronous stdin writes
  back-pressure video. The HLS muxer receives `-thread_queue_size 512`; in
  ffmpeg n9.1 this is a muxer option, not an input option.
- **Codec coverage.** The same AAC branch is emitted alongside software and
  hardware H.264/HEVC/AV1 video, MPEG-TS segments, and the AV1 fragmented-MP4
  segment path.
- **Video-only argv.** Without `audio`, the argv still has `-an` and no second
  input.
- **`/video` stays video-only.** The live MPEG-TS/fMP4 response uses the shared
  encoder construction with audio disabled, so enabling HLS audio does not
  change the `/video` contract.

## 3. Failure behavior

- `--hls-audio` with no explicit source and no named sink is a typed startup
  error (`HlsError::AudioSource` / an operator-facing `--hls-audio-source`
  message). The node does not fall back to the default source.
- A named monitor that does not exist fails the ffmpeg generation. HLS status
  becomes `failed` (the manifest is not written / no keyframe), never
  `running` with invented audio.
- Audio and video share one ffmpeg process. A mid-stream audio failure stops
  that generation; there is no in-place video-only fallback after startup.
- No status field claims samples, a negotiated track, or a successful monitor
  open.

## 4. Status

`/api/v1/connection` adds configuration-intent fields to the existing HLS
surface:

```json
{
  "hls": "running",
  "hls_error": null,
  "hls_audio_configured": true,
  "hls_audio_source": "ozma-vm1.monitor",
  "hls_audio_encoder": "aac"
}
```

`multi_console_hls` reports aggregate intent and per-console intent:

```json
{
  "multi_console_hls": {
    "enabled": true,
    "audio_configured": true,
    "audio_source_template": "ozma-vm{console}.monitor",
    "audio_encoder": "aac",
    "consoles": [
      {
        "console": 0,
        "status": "running",
        "audio_configured": true,
        "audio_source": "ozma-vm0.monitor",
        "audio_encoder": "aac"
      }
    ]
  }
}
```

These fields describe configuration only. The source monitor names are not
secret, and no unconfigured console reports audio.

## 5. Tests

Unit tests in `ozma-node/src/hls.rs`:

- `ffmpeg_argv_adds_a_pulse_monitor_and_aac_when_audio_is_configured` — Pulse
  input, explicit stream maps, `wallclock=0`, bounded fragment size, AAC
  encoder/bitrate, resampler alignment, and no `-an`.
- `video_only_argv_has_no_audio_input_even_when_audio_is_configured` — HLS
  without audio still uses `-an`; `/video` MPEG-TS/fMP4 stay video-only even
  when the config carries audio.
- `audio_configuration_rejects_blank_sources_and_unbounded_bitrates` — typed
  source/bitrate validation.
- `status_reports_audio_intent_without_claiming_samples` — status intent only.

`ozma-node/src/multi_hls.rs`:

- `audio_template_expands_console_and_index_and_rejects_unknown_placeholders`.
- `per_console_audio_template_scopes_each_pipeline_and_status` — console 0 and
  1 receive `ozma-vm0.monitor` / `ozma-vm1.monitor`, each reports `aac`.
- `per_console_audio_without_template_stays_video_only`.

`ozma-node/src/controller_compat.rs`:

- `connection_reports_hls_audio_intent_without_sample_claims` — top-level
  `hls_audio_*` fields and the video-only false/null shape.

`ozma-node/src/main.rs`:

- `hls_audio_source_is_explicit_or_derived_from_a_named_monitor` — explicit
  source wins; `--audio-sink` and `monitor:<sink>` derive `<sink>.monitor`; a
  request without a named sink is a typed error; flags parse.

Live test (opt-in, disposable sink only):

```bash
OZMA_FFMPEG_AUDIO_TEST=1 \
  cargo test -p ozma-node --lib real_ffmpeg_muxes_a_monitor_tone \
  -- --ignored --nocapture
```

Observed on 2026-11-26 (PulseAudio on PipeWire 1.6.11, ffmpeg n9.1):

```text
h264 segment /tmp/ozma-hls-real-audio-h264-544241-0/seg_00000.ts:
codec_name=h264
codec_type=video
duration=0.500000
codec_name=aac
codec_type=audio
duration=0.384000
hevc segment /tmp/ozma-hls-real-audio-hevc-544241-1/seg_00000.ts:
codec_name=hevc
codec_type=video
duration=0.500000
codec_name=aac
codec_type=audio
duration=0.384000
test result: ok. 1 passed; 0 failed; 0 ignored
```

The test creates `ozma-hls-audio-<pid>` with `module-null-sink`, plays a
generated 440 Hz sine into it, waits for `<sink>.monitor`, feeds a real-time
fake RGB24 source through `HlsPipeline`, and unloads the module in a guard. It
never reads or writes the default sink and leaves no ffmpeg or sink behind.

Gates:

```text
cargo test -p ozma-node --lib
  802 passed; 0 failed; 8 ignored
cargo check -p ozma-node --all-targets
  PASS
rustfmt on the four touched ozma-node sources
  PASS
```

## 6. Remaining gaps

- Only ffmpeg's `pulse` input is emitted. A native PipeWire-only ffmpeg build
  without the Pulse compatibility server is not auto-detected or retried with
  `-f pipewire`.
- A named-but-missing monitor fails the generation but its ffmpeg stderr is
  not captured into the status string today; operators see the HLS `failed`
  state and manifest/keyframe error, not the exact Pulse error. The startup
  "no source configured" case is typed and explicit.
- Audio is muxed by the same ffmpeg process as video. There is no automatic
  restart with audio disabled after a successful start.
- Only AAC is offered; no Opus, AC-3, or multi-track selection.
- A base monitor's AAC bitrate wins for template-expanded consoles; there is
  no separate per-console bitrate override.
- A/V alignment is ffmpeg zero-epoch plus `aresample` drift correction. There
  is no independent jitter/latency measurement or sample-level evidence in
  status.
