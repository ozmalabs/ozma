// SPDX-License-Identifier: AGPL-3.0-only
//! H.264 video decode with WebCodecs.
//!
//! The node sends one complete Annex B access unit per [`MediaPacket`]. This
//! module probes for a hardware H.264 decoder, feeds `EncodedVideoChunk`s with
//! a monotonic microsecond timestamp derived from the wire sequence, and draws
//! decoded frames on the canvas.
//!
//! Recovery policy: a decoder error resets the decoder, drops input until the
//! next keyframe, and increments `videoErrors`. No loop retries the same bytes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    EncodedVideoChunk, EncodedVideoChunkInit, EncodedVideoChunkType, HardwareAcceleration,
    HtmlCanvasElement, VideoDecoder, VideoDecoderConfig, VideoDecoderInit, VideoFrame,
};

use crate::canvas::CanvasSurface;
use crate::error;
use crate::stats::Stats;

/// H.264 codec strings to probe, in preference order.
///
/// `avc1.42E01E` is Baseline 3.1 (the most widely decodable), `avc1.4D401F` is
/// Main 3.1, and `avc1.640028` is High 4.0. The node's encoder should advertise
/// one of these profiles; probing them covers the common hardware decoders.
pub const H264_CANDIDATES: [&str; 3] = ["avc1.42E01E", "avc1.4D401F", "avc1.640028"];

/// Assumed frame interval when the timestamp is derived from a sequence.
///
/// The protocol does not carry a presentation timestamp on the video channel,
/// so the client derives one from the access-unit sequence at 30 fps. WebCodecs
/// only requires monotonic timestamps for ordering; it does not use them for
/// wall-clock pacing.
const FRAME_INTERVAL_MICROS: f64 = 33_333.0;

pub struct VideoPipeline {
    decoder: VideoDecoder,
    config: VideoDecoderConfig,
    needs_keyframe: bool,
    needs_configure: bool,
    error_flag: Rc<Cell<bool>>,
    stats: Rc<Stats>,
    // The decoder owns the JS functions; these closures must outlive it.
    _output: Closure<dyn FnMut(VideoFrame)>,
    _error: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
}

impl VideoPipeline {
    /// Probe for a supported H.264 configuration and create a decoder.
    pub async fn new(
        canvas: HtmlCanvasElement,
        stats: Rc<Stats>,
    ) -> Result<Self, wasm_bindgen::JsValue> {
        let config = probe_h264_config().await?;
        let surface = Rc::new(RefCell::new(CanvasSurface::new(canvas, stats.clone())?));
        let error_flag = Rc::new(Cell::new(false));

        let output_surface = surface.clone();
        let output = Closure::<dyn FnMut(VideoFrame)>::new(move |frame: VideoFrame| {
            if let Ok(mut surface) = output_surface.try_borrow_mut() {
                surface.draw(frame);
            } else {
                frame.close();
            }
        });

        let failure_flag = error_flag.clone();
        let on_error = Closure::<dyn FnMut(wasm_bindgen::JsValue)>::new(
            move |_error: wasm_bindgen::JsValue| {
                // Only flag the error here; resetting is done on the next push so
                // no decoder state is mutated re-entrantly from a callback.
                failure_flag.set(true);
            },
        );

        let init = VideoDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            output.as_ref().unchecked_ref(),
        );
        let decoder = VideoDecoder::new(&init).map_err(|value| {
            error::from_js(value, error::UNSUPPORTED, "creating the H.264 decoder")
        })?;
        if let Err(first) = decoder.configure(&config) {
            // A few implementations insist on coded dimensions even though the
            // WebCodecs dictionary makes them optional. Retry once with a
            // conventional 1080p hint; the SPS still drives the real size.
            config.set_coded_width(1920);
            config.set_coded_height(1080);
            decoder.configure(&config).map_err(|second| {
                error::from_js(
                    second,
                    error::UNSUPPORTED,
                    &format!(
                        "configuring the H.264 decoder (first attempt without coded size failed: {first:?})"
                    ),
                )
            })?;
        }

        Ok(Self {
            decoder,
            config,
            needs_keyframe: true,
            needs_configure: false,
            error_flag,
            stats,
            _output: output,
            _error: on_error,
        })
    }

    /// Present one complete encoded access unit.
    ///
    /// `keyframe` comes from the wire fragment header. Frames are dropped while
    /// the decoder waits for a keyframe after startup or an error.
    pub fn push(
        &mut self,
        sequence: u64,
        keyframe: bool,
        data: &[u8],
    ) -> Result<(), wasm_bindgen::JsValue> {
        if data.is_empty() {
            return Ok(());
        }
        if self.error_flag.replace(false) {
            self.stats
                .video_errors
                .set(self.stats.video_errors.get() + 1);
            // Clearing the control message queue and decoder state is required
            // before a new keyframe; keep the configuration to reapply it.
            let _ = self.decoder.reset();
            self.needs_configure = true;
            self.needs_keyframe = true;
        }

        if self.needs_keyframe && !keyframe {
            self.stats
                .video_dropped
                .set(self.stats.video_dropped.get() + 1);
            return Ok(());
        }

        if self.needs_configure {
            self.decoder.configure(&self.config).map_err(|value| {
                error::from_js(value, error::UNSUPPORTED, "reconfiguring the H.264 decoder")
            })?;
            self.needs_configure = false;
        }
        self.needs_keyframe = false;

        let chunk_type = if keyframe {
            EncodedVideoChunkType::Key
        } else {
            EncodedVideoChunkType::Delta
        };
        let bytes = js_sys::Uint8Array::from(data);
        let init =
            EncodedVideoChunkInit::new(bytes.unchecked_ref::<js_sys::Object>(), 0, chunk_type);
        init.set_timestamp_f64(sequence as f64 * FRAME_INTERVAL_MICROS);
        let chunk = match EncodedVideoChunk::new(&init) {
            Ok(chunk) => chunk,
            Err(value) => {
                self.needs_keyframe = true;
                return Err(error::from_js(
                    value,
                    error::UNSUPPORTED,
                    "building an encoded video chunk",
                ));
            }
        };
        if let Err(value) = self.decoder.decode(&chunk) {
            self.stats
                .video_errors
                .set(self.stats.video_errors.get() + 1);
            let _ = self.decoder.reset();
            self.needs_configure = true;
            self.needs_keyframe = true;
            return Err(error::from_js(
                value,
                error::UNSUPPORTED,
                "decoding a video access unit",
            ));
        }
        Ok(())
    }

    /// Close the decoder when the session ends.
    pub fn close(&self) {
        let _ = self.decoder.close();
    }
}

/// Probe the H.264 candidates, preferring hardware and low latency.
///
/// `VideoDecoderConfig.codedWidth`/`codedHeight` are omitted: the configuration
/// is valid without them, the decoder derives the size from the SPS, and the
/// canvas adopts the first decoded frame's display size. No manual SPS parsing
/// is required.
async fn probe_h264_config() -> Result<VideoDecoderConfig, wasm_bindgen::JsValue> {
    for codec in H264_CANDIDATES {
        for hardware in [
            HardwareAcceleration::PreferHardware,
            HardwareAcceleration::NoPreference,
        ] {
            let config = candidate(codec, hardware);
            let supported = VideoDecoder::is_config_supported(&config);
            let support = match JsFuture::from(supported).await {
                Ok(value) => value,
                Err(value) => {
                    return Err(error::from_js(
                        value,
                        error::UNSUPPORTED,
                        "probing WebCodecs H.264 support",
                    ))
                }
            };
            // `VideoDecoderSupport` is a Web IDL dictionary, not an interface.
            // `wasm_bindgen`'s `dyn_into` performs an `instanceof` check, which
            // is always false for a plain dictionary object, so read the
            // property directly instead of casting.
            let supported = js_sys::Reflect::get(&support, &JsValue::from_str("supported"))
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if supported {
                return Ok(config);
            }
        }
    }
    Err(error::client_error(
        error::UNSUPPORTED,
        "This browser has no supported WebCodecs H.264 decoder (tried avc1.42E01E, avc1.4D401F, avc1.640028)",
    ))
}

fn candidate(codec: &str, hardware: HardwareAcceleration) -> VideoDecoderConfig {
    let config = VideoDecoderConfig::new(codec);
    config.set_hardware_acceleration(hardware);
    config.set_optimize_for_latency(true);
    // `avc` is a codec registration extension. web-sys has no typed field for
    // it yet, so set the dictionary member reflectively. Annex B is what the
    // node sends; without this an `avc` decoder defaults to AVCC/length-prefixed
    // input in some implementations.
    let avc = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &avc,
        &wasm_bindgen::JsValue::from_str("format"),
        &wasm_bindgen::JsValue::from_str("annexb"),
    );
    let _ = js_sys::Reflect::set(
        config.as_ref(),
        &wasm_bindgen::JsValue::from_str("avc"),
        &avc,
    );
    config
}
