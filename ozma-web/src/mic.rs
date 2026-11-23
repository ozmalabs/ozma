// SPDX-License-Identifier: AGPL-3.0-only
//! Browser microphone uplink: `getUserMedia` → Web Audio → WebCodecs Opus →
//! one raw Opus packet per datagram on [`ChannelKind::Mic`].
//!
//! The graph is `MediaStreamAudioSourceNode → ScriptProcessorNode →
//! destination`. The source node is forced to `channelCount = 2` with
//! `channelCountMode = "explicit"`, so a mono fake microphone is upmixed and
//! the processor always sees two channels. The processor only reads its input
//! buffer; its output stays silent, so capturing does not echo into the
//! speakers. 960-frame (20 ms) f32-planar stereo blocks are handed to an Opus
//! encoder at 64 kbps.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use bytes::Bytes;
use ozma_protocol::ApiError;
use ozma_transport::ChannelId;
use ozma_transport_web::BrowserSender;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    AudioContext, AudioContextOptions, AudioContextState, AudioData, AudioDataInit,
    AudioEncoder, AudioEncoderConfig, AudioEncoderInit, AudioProcessingEvent, AudioSampleFormat,
    ChannelCountMode, ConstrainLongRange, MediaStream, MediaStreamAudioSourceNode,
    MediaStreamAudioSourceOptions, MediaStreamConstraints, MediaStreamTrack, MediaTrackConstraints,
    ScriptProcessorNode,
};

use crate::capture::CaptureInfo;
use crate::error;
use crate::stats::Stats;

const SAMPLE_RATE: f32 = 48_000.0;
const CHANNELS: u32 = 2;
const FRAMES_PER_PACKET: usize = 960;
const PACKET_MICROS: f64 = 20_000.0;
const BITRATE: u32 = 64_000;
const PROCESSOR_BUFFER: u32 = 4096;

/// A running microphone encoder.
pub struct MicPipeline {
    stream: MediaStream,
    context: AudioContext,
    source: MediaStreamAudioSourceNode,
    processor: ScriptProcessorNode,
    state: Rc<RefCell<MicState>>,
    // The encoder and processor own these JS functions; keep the Rust closures
    // alive for as long as the pipeline.
    _process: Closure<dyn FnMut(AudioProcessingEvent)>,
    _output: Closure<dyn FnMut(web_sys::EncodedAudioChunk)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

impl MicPipeline {
    /// Request the microphone and start encoding. Must be called from a user
    /// gesture so the `AudioContext` may resume.
    pub async fn start(
        sender: BrowserSender,
        control: ChannelId,
        stats: Rc<Stats>,
        capture: Rc<CaptureInfo>,
    ) -> Result<Self, JsValue> {
        match Self::start_inner(&sender, control, &stats, &capture).await {
            Ok(pipeline) => Ok(pipeline),
            Err(api) => {
                capture.set_audio(false);
                capture.set_error(api.clone());
                capture.send_state(&sender, control);
                Err(error::from_api_error(&api))
            }
        }
    }

    async fn start_inner(
        sender: &BrowserSender,
        control: ChannelId,
        stats: &Rc<Stats>,
        capture: &Rc<CaptureInfo>,
    ) -> Result<Self, ApiError> {
        let window = web_sys::window().ok_or_else(|| {
            ApiError::new(
                error::CAPTURE_FAILED,
                "The microphone pipeline needs a browser window.",
            )
        })?;
        let media = window.navigator().media_devices().map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "opening media devices")
        })?;

        let constraints = MediaStreamConstraints::new();
        let audio_constraints = MediaTrackConstraints::new();
        let channels = ConstrainLongRange::new();
        channels.set_ideal(CHANNELS as i32);
        audio_constraints.set_channel_count_constrain_long_range(&channels);
        audio_constraints.set_echo_cancellation_bool(true);
        audio_constraints.set_noise_suppression_bool(true);
        audio_constraints.set_auto_gain_control_bool(true);
        // `MediaTrackConstraints` has no typed `sampleRate` member in
        // web-sys 0.3.95; the constraint is still valid as a dictionary field.
        let sample_rate = ConstrainLongRange::new();
        sample_rate.set_ideal(SAMPLE_RATE as i32);
        let _ = js_sys::Reflect::set(
            audio_constraints.as_ref(),
            &JsValue::from_str("sampleRate"),
            sample_rate.as_ref(),
        );
        constraints.set_audio_media_track_constraints(&audio_constraints);
        constraints.set_video_bool(false);

        let promise = media
            .get_user_media_with_constraints(&constraints)
            .map_err(|value| {
                error::api_from_js(&value, error::CAPTURE_FAILED, "requesting the microphone")
            })?;
        let value = JsFuture::from(promise).await.map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "requesting the microphone")
        })?;
        let stream: MediaStream = value.dyn_into().map_err(|_| {
            ApiError::new(
                error::CAPTURE_FAILED,
                "getUserMedia did not return a MediaStream",
            )
        })?;
        // Release the microphone and context if setup fails after this point.
        let guard = MicGuard {
            stream: stream.clone(),
            context: RefCell::new(None),
            armed: true,
        };

        let options = AudioContextOptions::new();
        options.set_sample_rate(SAMPLE_RATE);
        let context = AudioContext::new_with_context_options(&options).map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "creating the microphone context")
        })?;
        *guard.context.borrow_mut() = Some(context.clone());
        match context.resume() {
            Ok(promise) => {
                if let Err(value) = JsFuture::from(promise).await {
                    if context.state() == AudioContextState::Suspended {
                        stop_tracks(&stream);
                        let _ = context.close();
                        return Err(error::api_from_js(
                            &value,
                            error::CAPTURE_FAILED,
                            "resuming the microphone context",
                        ));
                    }
                }
            }
            Err(value) => {
                if context.state() == AudioContextState::Suspended {
                    stop_tracks(&stream);
                    let _ = context.close();
                    return Err(error::api_from_js(
                        &value,
                        error::CAPTURE_FAILED,
                        "resuming the microphone context",
                    ));
                }
            }
        }

        let source = MediaStreamAudioSourceNode::new(
            &context,
            &MediaStreamAudioSourceOptions::new(&stream),
        )
        .map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "creating the microphone source")
        })?;
        source.set_channel_count(CHANNELS);
        source.set_channel_count_mode(ChannelCountMode::Explicit);

        let processor = context
            .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
                PROCESSOR_BUFFER,
                CHANNELS,
                CHANNELS,
            )
            .map_err(|value| {
                error::api_from_js(
                    &value,
                    error::CAPTURE_FAILED,
                    "creating the microphone processor",
                )
            })?;
        source
            .connect_with_audio_node(&processor)
            .map_err(|value| {
                error::api_from_js(&value, error::CAPTURE_FAILED, "connecting the microphone source")
            })?;
        processor
            .connect_with_audio_node(&context.destination())
            .map_err(|value| {
                error::api_from_js(
                    &value,
                    error::CAPTURE_FAILED,
                    "connecting the microphone processor",
                )
            })?;

        let config = AudioEncoderConfig::new("opus", CHANNELS, SAMPLE_RATE as u32);
        config.set_bitrate(BITRATE);
        let promise = AudioEncoder::is_config_supported(&config);
        let support = JsFuture::from(promise).await.map_err(|value| {
            error::api_from_js(
                &value,
                error::CAPTURE_UNSUPPORTED,
                "probing WebCodecs Opus encoding",
            )
        })?;
        let supported = js_sys::Reflect::get(&support, &JsValue::from_str("supported"))
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if !supported {
            let _ = context.close();
            stop_tracks(&stream);
            return Err(ApiError::new(
                error::CAPTURE_UNSUPPORTED,
                "This browser has no supported WebCodecs Opus encoder",
            )
            .with_remediation(
                "Use a browser build with WebCodecs audio encoding enabled, or share the camera only.",
            ));
        }

        let output_state = Rc::new(MicOutput {
            sender: sender.clone(),
            control,
            channel: sender
                .channel_for_kind(ozma_protocol::ChannelKind::Mic)
                .ok_or_else(|| {
                    ApiError::new(
                        error::CAPTURE_UNSUPPORTED,
                        "The negotiated channel map has no microphone channel; set audio.mic = true in the session spec",
                    )
                })?,
            stats: stats.clone(),
            capture: capture.clone(),
            fatal: Rc::new(Cell::new(false)),
        });
        let output_closure = {
            let state = output_state.clone();
            Closure::<dyn FnMut(web_sys::EncodedAudioChunk)>::new(move |chunk| {
                state.on_chunk(chunk);
            })
        };
        let error_closure = {
            let state = output_state.clone();
            Closure::<dyn FnMut(JsValue)>::new(move |value| {
                state.on_encoder_error(value);
            })
        };
        let init = AudioEncoderInit::new(
            error_closure.as_ref().unchecked_ref(),
            output_closure.as_ref().unchecked_ref(),
        );
        let encoder = AudioEncoder::new(&init).map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_UNSUPPORTED, "creating the Opus encoder")
        })?;
        if let Err(value) = encoder.configure(&config) {
            let api = error::api_from_js(
                &value,
                error::CAPTURE_UNSUPPORTED,
                "configuring the Opus encoder",
            );
            let _ = encoder.close();
            let _ = context.close();
            stop_tracks(&stream);
            return Err(api);
        }

        let state = Rc::new(RefCell::new(MicState {
            encoder,
            stats: stats.clone(),
            capture: capture.clone(),
            left: Vec::new(),
            right: Vec::new(),
            timestamp: 0.0,
            stopped: false,
            fatal: output_state.fatal.clone(),
        }));

        let process_state = state.clone();
        let process = Closure::<dyn FnMut(AudioProcessingEvent)>::new(move |event| {
            if let Ok(mut state) = process_state.try_borrow_mut() {
                state.on_audio(event);
            }
        });
        processor.set_onaudioprocess(Some(process.as_ref().unchecked_ref()));
        capture.set_audio(true);

        guard.disarm();
        Ok(Self {
            stream,
            context,
            source,
            processor,
            state,
            _process: process,
            _output: output_closure,
            _error: error_closure,
        })
    }

    /// Stop processing, release the device, and close the encoder. Idempotent.
    pub fn close(&self) {
        self.processor.set_onaudioprocess(None);
        if let Ok(mut state) = self.state.try_borrow_mut() {
            if state.stopped {
                return;
            }
            state.stopped = true;
            let _ = state.encoder.close();
        }
        let _ = self.source.disconnect();
        let _ = self.processor.disconnect();
        stop_tracks(&self.stream);
        let _ = self.context.close();
    }
}

impl Drop for MicPipeline {
    fn drop(&mut self) {
        self.close();
    }
}

/// The encoder output callback state.
struct MicOutput {
    sender: BrowserSender,
    control: ChannelId,
    channel: ChannelId,
    stats: Rc<Stats>,
    capture: Rc<CaptureInfo>,
    fatal: Rc<Cell<bool>>,
}

impl MicOutput {
    fn on_chunk(&self, chunk: web_sys::EncodedAudioChunk) {
        let length = chunk.byte_length() as usize;
        if length == 0 {
            return;
        }
        let mut data = vec![0u8; length];
        if chunk.copy_to_with_u8_slice(&mut data).is_err() {
            self.on_error("EncodedAudioChunk.copyTo failed");
            return;
        }
        Stats::increment(&self.stats.mic_packets);
        self.stats.add_mic_bytes(data.len());
        let sender = self.sender.clone();
        let channel = self.channel;
        let stats = self.stats.clone();
        spawn_local(async move {
            if sender.send(channel, Bytes::from(data)).await.is_err() {
                Stats::increment(&stats.mic_errors);
            }
        });
    }

    fn on_encoder_error(&self, value: JsValue) {
        let api = error::api_from_js(&value, error::CAPTURE_FAILED, "Opus encoder error");
        self.fatal.set(true);
        self.record(&api);
    }

    fn on_error(&self, message: &str) {
        self.record(&ApiError::new(error::CAPTURE_FAILED, message));
    }

    fn record(&self, api: &ApiError) {
        Stats::increment(&self.stats.mic_errors);
        self.capture.set_error(api.clone());
        self.capture.send_state(&self.sender, self.control);
    }
}

/// PCM accumulation and encoder submission.
struct MicState {
    encoder: AudioEncoder,
    stats: Rc<Stats>,
    capture: Rc<CaptureInfo>,
    left: Vec<f32>,
    right: Vec<f32>,
    timestamp: f64,
    stopped: bool,
    fatal: Rc<Cell<bool>>,
}

impl MicState {
    fn on_audio(&mut self, event: AudioProcessingEvent) {
        if self.stopped || self.fatal.get() {
            return;
        }
        let Ok(buffer) = event.input_buffer() else {
            return;
        };
        let Ok(left) = buffer.get_channel_data(0) else {
            Stats::increment(&self.stats.mic_errors);
            return;
        };
        let right = if buffer.number_of_channels() > 1 {
            buffer.get_channel_data(1).unwrap_or_else(|_| left.clone())
        } else {
            left.clone()
        };
        self.left.extend_from_slice(&left);
        self.right.extend_from_slice(&right);

        while self.left.len() >= FRAMES_PER_PACKET && self.right.len() >= FRAMES_PER_PACKET {
            if self.encoder.encode_queue_size() > 8 {
                // The encoder is behind; drop this block rather than growing
                // the PCM accumulator without bound.
                self.left.drain(..FRAMES_PER_PACKET);
                self.right.drain(..FRAMES_PER_PACKET);
                Stats::increment(&self.stats.mic_errors);
                continue;
            }
            let mut planar = Vec::with_capacity(FRAMES_PER_PACKET * CHANNELS as usize);
            planar.extend_from_slice(&self.left[..FRAMES_PER_PACKET]);
            planar.extend_from_slice(&self.right[..FRAMES_PER_PACKET]);
            self.left.drain(..FRAMES_PER_PACKET);
            self.right.drain(..FRAMES_PER_PACKET);

            let samples = js_sys::Float32Array::from(planar.as_slice());
            let init = AudioDataInit::new(
                samples.unchecked_ref::<js_sys::Object>(),
                AudioSampleFormat::F32Planar,
                CHANNELS,
                FRAMES_PER_PACKET as u32,
                SAMPLE_RATE,
                0,
            );
            init.set_timestamp_f64(self.timestamp);
            self.timestamp += PACKET_MICROS;
            let data = match AudioData::new(&init) {
                Ok(data) => data,
                Err(value) => {
                    Stats::increment(&self.stats.mic_errors);
                    let api = error::api_from_js(
                        &value,
                        error::CAPTURE_FAILED,
                        "building microphone AudioData",
                    );
                    self.capture.set_error(api);
                    continue;
                }
            };
            let result = self.encoder.encode(&data);
            data.close();
            if result.is_err() {
                Stats::increment(&self.stats.mic_errors);
                self.fatal.set(true);
            }
        }
    }
}

fn stop_tracks(stream: &MediaStream) {
    let tracks = stream.get_tracks();
    for index in 0..tracks.length() {
        if let Ok(track) = tracks.get(index).dyn_into::<MediaStreamTrack>() {
            track.stop();
        }
    }
}

/// Releases a microphone stream and audio context on any early return from
/// `MicPipeline::start_inner`.
struct MicGuard {
    stream: MediaStream,
    context: RefCell<Option<AudioContext>>,
    armed: bool,
}

impl MicGuard {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for MicGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        stop_tracks(&self.stream);
        if let Some(context) = self.context.borrow_mut().take() {
            let _ = context.close();
        }
    }
}
