// SPDX-License-Identifier: AGPL-3.0-only
//! Opus audio decode and playback with WebCodecs + Web Audio.
//!
//! Opus packets on the audio channel are complete, self-delimiting packets. The
//! decoder runs at 48 kHz stereo (the protocol's [`AudioFormat::opus`]); decoded
//! `AudioData` planes are copied into an `AudioBuffer` and scheduled with a
//! small lead time. Packets that fall more than a quarter second behind the
//! audio clock are dropped and counted so a stalled tab does not replay stale
//! audio.
//!
//! [`AudioFormat::opus`]: ozma_protocol::media::AudioFormat::opus

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{
    AudioBufferSourceNode, AudioContext, AudioContextState, AudioData, AudioDataCopyToOptions,
    AudioDecoder, AudioDecoderConfig, AudioDecoderInit, AudioSampleFormat, EncodedAudioChunk,
    EncodedAudioChunkInit, EncodedAudioChunkType,
};

use crate::error;
use crate::stats::Stats;

/// Scheduling lead: start slightly in the future so the audio clock can serve
/// the first samples rather than underrunning immediately.
const LEAD_SECONDS: f64 = 0.060;

/// Drop audio that is further behind than this.
const MAX_BEHIND_SECONDS: f64 = 0.250;

/// Opus packets are commonly 20 ms. The protocol does not carry packet
/// duration, so the decoder timestamp is derived from a 20 ms increment; only
/// monotonicity matters to WebCodecs.
const OPUS_PACKET_MICROS: f64 = 20_000.0;

pub struct AudioPipeline {
    context: AudioContext,
    decoder: AudioDecoder,
    config: AudioDecoderConfig,
    needs_configure: bool,
    error_flag: Rc<Cell<bool>>,
    stats: Rc<Stats>,
    timestamp_micros: f64,
    _output: Closure<dyn FnMut(AudioData)>,
    _error: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
}

impl AudioPipeline {
    /// Create the decoder and audio graph.
    ///
    /// The `AudioContext` may start suspended because no user gesture has
    /// happened yet; [`AudioPipeline::resume`] is safe to call repeatedly and
    /// is also attempted on the first decoded packet.
    pub fn new(stats: Rc<Stats>) -> Result<Self, wasm_bindgen::JsValue> {
        let context = AudioContext::new().map_err(|value| {
            error::from_js(value, error::UNSUPPORTED, "creating the audio context")
        })?;
        let config = AudioDecoderConfig::new("opus", 2, 48_000);

        let playback = Rc::new(Playback {
            context: context.clone(),
            stats: stats.clone(),
            next_start: Cell::new(0.0),
        });

        let output_context = playback.clone();
        let output = Closure::<dyn FnMut(AudioData)>::new(move |data: AudioData| {
            output_context.play(data);
        });

        let error_flag = Rc::new(Cell::new(false));
        let failure_flag = error_flag.clone();
        let on_error = Closure::<dyn FnMut(wasm_bindgen::JsValue)>::new(
            move |_error: wasm_bindgen::JsValue| {
                failure_flag.set(true);
            },
        );

        let init = AudioDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );
        let decoder = AudioDecoder::new(&init).map_err(|value| {
            error::from_js(value, error::UNSUPPORTED, "creating the Opus decoder")
        })?;
        decoder.configure(&config).map_err(|value| {
            error::from_js(value, error::UNSUPPORTED, "configuring the Opus decoder")
        })?;

        Ok(Self {
            context,
            decoder,
            config,
            needs_configure: false,
            error_flag,
            stats,
            timestamp_micros: 0.0,
            _output: output,
            _error: on_error,
        })
    }

    /// Resume a suspended audio context. Browsers require a user gesture, so
    /// call this from a pointer or key handler.
    pub fn resume(&self) -> Result<(), wasm_bindgen::JsValue> {
        if self.context.state() == AudioContextState::Suspended {
            let _ = self.context.resume().map_err(|value| {
                error::from_js(value, error::UNSUPPORTED, "resuming the audio context")
            })?;
        }
        Ok(())
    }

    pub fn is_suspended(&self) -> bool {
        self.context.state() == AudioContextState::Suspended
    }

    /// Close the audio context when the session ends.
    pub fn close(&self) {
        let _ = self.context.close();
    }

    /// Decode one complete Opus packet.
    pub fn push(&mut self, data: &[u8]) -> Result<(), wasm_bindgen::JsValue> {
        if data.is_empty() {
            return Ok(());
        }
        // A user gesture may already have happened; if not this is a no-op
        // until the demo calls resume() from a handler.
        if self.is_suspended() {
            let _ = self.context.resume();
        }
        if self.error_flag.replace(false) {
            self.stats
                .audio_errors
                .set(self.stats.audio_errors.get() + 1);
            let _ = self.decoder.reset();
            self.needs_configure = true;
        }
        if self.needs_configure {
            self.decoder.configure(&self.config).map_err(|value| {
                error::from_js(value, error::UNSUPPORTED, "reconfiguring the Opus decoder")
            })?;
            self.needs_configure = false;
        }

        let bytes = js_sys::Uint8Array::from(data);
        let init = EncodedAudioChunkInit::new(
            bytes.unchecked_ref::<js_sys::Object>(),
            0,
            EncodedAudioChunkType::Key,
        );
        init.set_timestamp_f64(self.timestamp_micros);
        self.timestamp_micros += OPUS_PACKET_MICROS;
        let chunk = EncodedAudioChunk::new(&init).map_err(|value| {
            error::from_js(value, error::UNSUPPORTED, "building an encoded audio chunk")
        })?;
        if let Err(value) = self.decoder.decode(&chunk) {
            self.stats
                .audio_errors
                .set(self.stats.audio_errors.get() + 1);
            let _ = self.decoder.reset();
            self.needs_configure = true;
            return Err(error::from_js(
                value,
                error::UNSUPPORTED,
                "decoding an Opus packet",
            ));
        }
        Ok(())
    }
}

/// Shared audio-clock state used by the output callback.
struct Playback {
    context: AudioContext,
    stats: Rc<Stats>,
    next_start: Cell<f64>,
}

impl Playback {
    fn play(&self, data: AudioData) {
        let frames = data.number_of_frames();
        let channels = data.number_of_channels().min(2);
        if frames == 0 || channels == 0 {
            data.close();
            return;
        }
        let sample_rate = if data.sample_rate() > 0.0 {
            data.sample_rate()
        } else {
            48_000.0
        };
        let now = self.context.current_time();
        let lead = LEAD_SECONDS;
        let mut next_start = self.next_start.get();
        if next_start <= 0.0 {
            next_start = now + lead;
        }
        if now - next_start > MAX_BEHIND_SECONDS {
            // The tab was throttled or the decoder fell behind; discard rather
            // than burst stale packets.
            self.stats
                .audio_dropped
                .set(self.stats.audio_dropped.get() + 1);
            self.next_start.set(now + lead);
            data.close();
            return;
        }

        let buffer = match self.context.create_buffer(channels, frames, sample_rate) {
            Ok(buffer) => buffer,
            Err(_) => {
                self.stats
                    .audio_dropped
                    .set(self.stats.audio_dropped.get() + 1);
                data.close();
                return;
            }
        };

        for channel in 0..channels {
            let options = AudioDataCopyToOptions::new(channel);
            options.set_format(AudioSampleFormat::F32Planar);
            let mut raw = vec![0u8; frames as usize * std::mem::size_of::<f32>()];
            if data.copy_to_with_u8_slice(&mut raw, &options).is_err() {
                self.stats
                    .audio_dropped
                    .set(self.stats.audio_dropped.get() + 1);
                data.close();
                return;
            }
            let samples: Vec<f32> = raw
                .chunks_exact(4)
                .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .collect();
            let samples = js_sys::Float32Array::from(samples.as_slice());
            if buffer
                .copy_to_channel_with_f32_array(&samples, channel as i32)
                .is_err()
            {
                self.stats
                    .audio_dropped
                    .set(self.stats.audio_dropped.get() + 1);
                data.close();
                return;
            }
        }
        data.close();

        let source = match self.context.create_buffer_source() {
            Ok(source) => source,
            Err(_) => {
                self.stats
                    .audio_dropped
                    .set(self.stats.audio_dropped.get() + 1);
                return;
            }
        };
        if self
            .schedule(&source, &buffer, next_start, now, frames, sample_rate)
            .is_err()
        {
            self.stats
                .audio_dropped
                .set(self.stats.audio_dropped.get() + 1);
        }
    }

    fn schedule(
        &self,
        source: &AudioBufferSourceNode,
        buffer: &web_sys::AudioBuffer,
        next_start: f64,
        now: f64,
        frames: u32,
        sample_rate: f32,
    ) -> Result<(), wasm_bindgen::JsValue> {
        source.set_buffer(Some(buffer));
        source
            .connect_with_audio_node(&self.context.destination())
            .map_err(|value| {
                error::from_js(value, error::UNSUPPORTED, "connecting the audio source")
            })?;
        let start = next_start.max(now + LEAD_SECONDS);
        source.start_with_when(start).map_err(|value| {
            error::from_js(value, error::UNSUPPORTED, "scheduling an audio buffer")
        })?;
        self.next_start
            .set(start + frames as f64 / sample_rate as f64);
        self.stats
            .audio_frames
            .set(self.stats.audio_frames.get() + 1);
        Ok(())
    }
}
