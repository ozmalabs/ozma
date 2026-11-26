// SPDX-License-Identifier: AGPL-3.0-only
//! Browser camera uplink: `getUserMedia` → WebCodecs H.264 → fragmented
//! datagrams on [`ChannelKind::Camera`].
//!
//! The node accepts Annex-B H.264 access units using the same
//! [`VideoFragmentHeader`] as the downstream video channels
//! (`ozma-node/src/client_capture.rs`). Frames are driven by a 30 Hz timer over
//! a hidden `<video>` element because `web-sys` 0.3.95 does not expose
//! `requestVideoFrameCallback`. `VideoEncoder.encode_queue_size()` bounds the
//! work: above two queued frames the capture drops the newest frame and counts
//! `cameraDropped` instead of growing memory.
//!
//! If no H.264 configuration is supported, or the encoder produces
//! length-prefixed (AVCC) output instead of Annex B, the pipeline reports a
//! typed `capture.unsupported` error rather than sending bytes the node cannot
//! decode.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use bytes::Bytes;
use ozma_protocol::media::{VideoFragmentHeader, VIDEO_FRAGMENT_HEADER_LEN};
use ozma_protocol::ApiError;
use ozma_transport::ChannelId;
use ozma_transport_web::BrowserSender;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    ConstrainDoubleRange, ConstrainLongRange, Document, HardwareAcceleration, HtmlVideoElement,
    LatencyMode, MediaStream, MediaStreamConstraints, MediaStreamTrack, MediaTrackConstraints,
    VideoEncoder, VideoEncoderConfig, VideoEncoderEncodeOptions, VideoEncoderInit, VideoFrame,
    VideoFrameInit,
};

use crate::capture::CaptureInfo;
use crate::error;
use crate::stats::Stats;

/// Maximum fragment payload. The node's channel `max_payload` is 1200 bytes and
/// the transport adds the channel byte plus this header, so 1100 leaves room
/// even on a small negotiated datagram.
const MAX_FRAGMENT: usize = 1100;

/// Capture cadence. The camera track is requested at 30 fps.
const FRAME_INTERVAL_MS: i32 = 33;
const FRAME_INTERVAL_MICROS: f64 = 33_333.0;

/// H.264 profiles to probe, in preference order.
const H264_CANDIDATES: [&str; 3] = ["avc1.42E01E", "avc1.4D401F", "avc1.640028"];

const REQUESTED_WIDTH: u32 = 1280;
const REQUESTED_HEIGHT: u32 = 720;
const BITRATE: u32 = 2_500_000;

/// A running browser camera encoder.
pub struct CameraPipeline {
    stream: MediaStream,
    video: HtmlVideoElement,
    core: Rc<CameraLoop>,
    created_element: bool,
    // The encoder owns these JS functions; the closures must outlive it.
    _output: Closure<dyn FnMut(web_sys::EncodedVideoChunk)>,
    _error: Closure<dyn FnMut(JsValue)>,
}

impl CameraPipeline {
    /// Request the camera, probe H.264, and start pulling frames into the
    /// encoder. Must be called from a user gesture because it calls
    /// `getUserMedia`.
    pub async fn start(
        sender: BrowserSender,
        control: ChannelId,
        stats: Rc<Stats>,
        capture: Rc<CaptureInfo>,
    ) -> Result<Self, JsValue> {
        match Self::start_inner(&sender, control, &stats, &capture).await {
            Ok(pipeline) => Ok(pipeline),
            Err(api) => {
                capture.set_video(false);
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
                "The camera pipeline needs a browser window.",
            )
        })?;
        let document = window.document().ok_or_else(|| {
            ApiError::new(
                error::CAPTURE_FAILED,
                "The camera pipeline needs a document.",
            )
        })?;

        let media = window.navigator().media_devices().map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "opening media devices")
        })?;

        let constraints = MediaStreamConstraints::new();
        let video_constraints = MediaTrackConstraints::new();
        let width = ConstrainLongRange::new();
        width.set_ideal(REQUESTED_WIDTH as i32);
        video_constraints.set_width_constrain_long_range(&width);
        let height = ConstrainLongRange::new();
        height.set_ideal(REQUESTED_HEIGHT as i32);
        video_constraints.set_height_constrain_long_range(&height);
        let frame_rate = ConstrainDoubleRange::new();
        frame_rate.set_ideal(30.0);
        video_constraints.set_frame_rate_constrain_double_range(&frame_rate);
        constraints.set_video_media_track_constraints(&video_constraints);
        constraints.set_audio_bool(false);

        let promise = media
            .get_user_media_with_constraints(&constraints)
            .map_err(|value| {
                error::api_from_js(&value, error::CAPTURE_FAILED, "requesting the camera")
            })?;
        let value = JsFuture::from(promise).await.map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "requesting the camera")
        })?;
        let stream: MediaStream = value.dyn_into().map_err(|_| {
            ApiError::new(
                error::CAPTURE_FAILED,
                "getUserMedia did not return a MediaStream",
            )
        })?;

        let (video, created_element) = match prepare_video(&document, &stream) {
            Ok(element) => element,
            Err(value) => {
                stop_tracks(&stream);
                return Err(error::api_from_js(
                    &value,
                    error::CAPTURE_FAILED,
                    "preparing the camera element",
                ));
            }
        };
        // Any early return below releases the device and removes a temporary
        // preview element; the guard is disarmed only on success.
        let guard = CaptureGuard {
            stream: stream.clone(),
            video: video.clone(),
            created: created_element,
            armed: true,
        };

        let play = video.play().map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "starting camera playback")
        })?;
        JsFuture::from(play).await.map_err(|value| {
            error::api_from_js(&value, error::CAPTURE_FAILED, "starting camera playback")
        })?;

        let (codec, hardware) = match probe_h264().await? {
            Some(choice) => choice,
            None => {
                return Err(ApiError::new(
                    error::CAPTURE_UNSUPPORTED,
                    "This browser has no supported WebCodecs H.264 encoder (tried avc1.42E01E, avc1.4D401F, avc1.640028)",
                )
                .with_remediation(
                    "Use a browser/OS build with hardware H.264 encoding, or run the demo in Chromium.",
                ));
            }
        };
        capture.set_codec(&codec);

        let output_state = Rc::new(CameraOutput {
            sender: sender.clone(),
            control,
            channel: sender
                .channel_for_kind(ozma_protocol::ChannelKind::Camera)
                .ok_or_else(|| {
                    ApiError::new(
                        error::CAPTURE_UNSUPPORTED,
                        "The negotiated channel map has no camera channel",
                    )
                })?,
            stats: stats.clone(),
            capture: capture.clone(),
            sequence: Cell::new(0),
            queue: RefCell::new(VecDeque::new()),
            sending: Cell::new(false),
            force_keyframe: Cell::new(false),
            annexb_checked: Cell::new(false),
            annexb_failed: Cell::new(false),
            fatal: Cell::new(false),
            max_datagram: sender.max_datagram_size(),
        });

        let output_closure = {
            let state = output_state.clone();
            Closure::<dyn FnMut(web_sys::EncodedVideoChunk)>::new(move |chunk| {
                state.on_chunk(chunk);
            })
        };
        let error_closure = {
            let state = output_state.clone();
            Closure::<dyn FnMut(JsValue)>::new(move |value| {
                state.on_encoder_error(value);
            })
        };

        let init = VideoEncoderInit::new(
            error_closure.as_ref().unchecked_ref(),
            output_closure.as_ref().unchecked_ref(),
        );
        let encoder = VideoEncoder::new(&init).map_err(|value| {
            error::api_from_js(
                &value,
                error::CAPTURE_UNSUPPORTED,
                "creating the H.264 encoder",
            )
        })?;

        let (frame_width, frame_height) = layout_size(&video);
        let config = encoder_config_with(&codec, frame_width, frame_height, hardware);
        if let Err(value) = encoder.configure(&config) {
            let _ = encoder.close();
            return Err(error::api_from_js(
                &value,
                error::CAPTURE_UNSUPPORTED,
                "configuring the H.264 encoder",
            ));
        }

        let core = Rc::new(CameraLoop {
            video: video.clone(),
            encoder,
            output: output_state,
            stats: stats.clone(),
            capture: capture.clone(),
            stopped: Cell::new(false),
            timer: Cell::new(None),
            tick: RefCell::new(None),
            timestamp: Cell::new(0.0),
        });
        start_ticks(core.clone());
        let size = layout_size(&video);
        stats.camera_width.set(size.0);
        stats.camera_height.set(size.1);
        capture.set_size(size.0, size.1);

        guard.disarm();
        Ok(Self {
            stream,
            video,
            core,
            created_element,
            _output: output_closure,
            _error: error_closure,
        })
    }

    /// Force the next encoded frame to be an IDR.
    pub fn request_keyframe(&self) {
        self.core.output.request_keyframe();
    }

    /// Stop the timer, release the device, and close the encoder. Idempotent.
    pub fn close(&self) {
        if self.core.stopped.replace(true) {
            return;
        }
        if let Some(handle) = self.core.timer.take() {
            if let Some(window) = web_sys::window() {
                window.clear_timeout_with_handle(handle);
            }
        }
        self.core.tick.borrow_mut().take();
        let _ = self.core.encoder.close();
        stop_tracks(&self.stream);
        self.video.set_src_object(None);
        let _ = self.video.pause();
        remove_preview(&self.video, self.created_element);
    }
}

impl Drop for CameraPipeline {
    fn drop(&mut self) {
        self.close();
    }
}

/// Encoder output: copy each access unit, fragment it, and serialize sends so
/// two access units never interleave on the camera channel.
struct CameraOutput {
    sender: BrowserSender,
    control: ChannelId,
    channel: ChannelId,
    stats: Rc<Stats>,
    capture: Rc<CaptureInfo>,
    sequence: Cell<u64>,
    queue: RefCell<VecDeque<PendingFrame>>,
    sending: Cell<bool>,
    force_keyframe: Cell<bool>,
    annexb_checked: Cell<bool>,
    annexb_failed: Cell<bool>,
    fatal: Cell<bool>,
    max_datagram: usize,
}

struct PendingFrame {
    keyframe: bool,
    sequence: u64,
    data: Vec<u8>,
}

impl CameraOutput {
    fn request_keyframe(&self) {
        // A subsequent frame may already be in the encoder queue; setting this
        // flag makes the next frame submitted carry `keyFrame: true`.
        self.force_keyframe.set(true);
    }

    /// Clear the pending keyframe request and report whether one was set.
    fn take_keyframe_request(&self) -> bool {
        self.force_keyframe.replace(false)
    }

    fn on_chunk(self: &Rc<Self>, chunk: web_sys::EncodedVideoChunk) {
        let length = chunk.byte_length() as usize;
        if length == 0 {
            return;
        }
        let mut data = vec![0u8; length];
        if chunk.copy_to_with_u8_slice(&mut data).is_err() {
            self.record_error(
                error::CAPTURE_FAILED,
                "EncodedVideoChunk.copyTo failed",
                None,
            );
            return;
        }
        if !self.annexb_checked.replace(true) && !is_annexb(&data) {
            self.annexb_failed.set(true);
            self.fatal.set(true);
            self.record_error(
                error::CAPTURE_UNSUPPORTED,
                "The H.264 encoder produced length-prefixed (AVCC) output, but the node accepts Annex B only",
                Some("Use a browser build that honours VideoEncoderConfig.avc.format = \"annexb\"."),
            );
            return;
        }
        if self.annexb_failed.get() {
            return;
        }
        let keyframe = chunk.type_() == web_sys::EncodedVideoChunkType::Key;
        self.stats.add_camera_bytes(data.len());
        let sequence = self.sequence.get();
        self.sequence.set(sequence.wrapping_add(1));
        self.queue.borrow_mut().push_back(PendingFrame {
            keyframe,
            sequence,
            data,
        });
        if !self.sending.replace(true) {
            drain(self.clone());
        }
    }

    fn on_encoder_error(&self, value: JsValue) {
        let api = error::api_from_js(&value, error::CAPTURE_FAILED, "H.264 encoder error");
        self.fatal.set(true);
        self.record_error(&api.code, &api.message, api.remediation.as_deref());
    }

    fn record_error(&self, code: &str, message: &str, remediation: Option<&str>) {
        Stats::increment(&self.stats.camera_errors);
        let mut api = ApiError::new(code, message);
        if let Some(remediation) = remediation {
            api = api.with_remediation(remediation);
        }
        self.capture.set_error(api);
        self.capture.send_state(&self.sender, self.control);
    }

    fn fragment_capacity(&self) -> Option<usize> {
        let capacity = self
            .max_datagram
            .saturating_sub(1 + VIDEO_FRAGMENT_HEADER_LEN)
            .min(MAX_FRAGMENT);
        (capacity > 0).then_some(capacity)
    }
}

fn drain(state: Rc<CameraOutput>) {
    spawn_local(async move {
        loop {
            let next = state.queue.borrow_mut().pop_front();
            let Some(frame) = next else {
                state.sending.set(false);
                if state.queue.borrow().is_empty() {
                    return;
                }
                if state.sending.replace(true) {
                    return;
                }
                continue;
            };
            let Some(capacity) = state.fragment_capacity() else {
                state.record_error(
                    error::CAPTURE_FAILED,
                    "The negotiated datagram size is too small for a camera fragment",
                    None,
                );
                continue;
            };
            let count = frame.data.len().div_ceil(capacity);
            if count == 0 || count > usize::from(u16::MAX) {
                state.record_error(
                    error::CAPTURE_FAILED,
                    "The encoded camera frame is too large to fragment",
                    None,
                );
                continue;
            }
            let mut failed = false;
            for index in 0..count {
                let start = index * capacity;
                let end = (start + capacity).min(frame.data.len());
                let fragment = VideoFragmentHeader::new(
                    frame.keyframe,
                    0,
                    frame.sequence,
                    index as u16,
                    count as u16,
                )
                .encode_into(&frame.data[start..end]);
                if let Err(value) = state
                    .sender
                    .send(state.channel, Bytes::from(fragment))
                    .await
                {
                    state.record_error(
                        error::CAPTURE_FAILED,
                        &format!("sending a camera fragment: {value}"),
                        None,
                    );
                    failed = true;
                    break;
                }
            }
            if failed {
                // Keep the queue moving; the next frame may fit once the
                // transport recovers.
                continue;
            }
        }
    });
}

/// State for the 30 Hz capture timer.
struct CameraLoop {
    video: HtmlVideoElement,
    encoder: VideoEncoder,
    output: Rc<CameraOutput>,
    stats: Rc<Stats>,
    capture: Rc<CaptureInfo>,
    stopped: Cell<bool>,
    timer: Cell<Option<i32>>,
    tick: RefCell<Option<Closure<dyn FnMut()>>>,
    timestamp: Cell<f64>,
}

impl CameraLoop {
    fn tick(&self) {
        if self.stopped.get() || self.output.fatal.get() {
            return;
        }
        let width = self.video.video_width();
        let height = self.video.video_height();
        if self.video.ready_state() < 2 || width == 0 || height == 0 {
            return;
        }
        self.stats.camera_width.set(width);
        self.stats.camera_height.set(height);
        self.capture.set_size(width, height);
        if self.encoder.encode_queue_size() > 2 {
            Stats::increment(&self.stats.camera_dropped);
            return;
        }

        let init = VideoFrameInit::new();
        let timestamp = self.timestamp.get();
        init.set_timestamp_f64(timestamp);
        self.timestamp.set(timestamp + FRAME_INTERVAL_MICROS);
        let frame = match VideoFrame::new_with_html_video_element_and_video_frame_init(
            &self.video,
            &init,
        ) {
            Ok(frame) => frame,
            Err(_) => {
                Stats::increment(&self.stats.camera_dropped);
                return;
            }
        };

        let result = if self.output.take_keyframe_request() {
            let options = VideoEncoderEncodeOptions::new();
            options.set_key_frame(true);
            self.encoder.encode_with_options(&frame, &options)
        } else {
            self.encoder.encode(&frame)
        };
        frame.close();
        match result {
            Ok(()) => Stats::increment(&self.stats.camera_frames),
            Err(value) => {
                let api =
                    error::api_from_js(&value, error::CAPTURE_FAILED, "submitting a camera frame");
                self.output.fatal.set(true);
                self.output
                    .record_error(&api.code, &api.message, api.remediation.as_deref());
            }
        }
    }

    fn schedule(self: &Rc<Self>) {
        if self.stopped.get() || self.timer.get().is_some() {
            return;
        }
        let Some(window) = web_sys::window() else {
            return;
        };
        let tick = self.tick.borrow();
        let Some(tick) = tick.as_ref() else {
            return;
        };
        if let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            FRAME_INTERVAL_MS,
        ) {
            self.timer.set(Some(handle));
        }
    }
}

fn start_ticks(core: Rc<CameraLoop>) {
    let weak = Rc::downgrade(&core);
    let closure = Closure::<dyn FnMut()>::new(move || {
        let Some(core) = weak.upgrade() else {
            return;
        };
        core.timer.set(None);
        if core.stopped.get() || core.output.fatal.get() {
            return;
        }
        core.tick();
        core.schedule();
    });
    *core.tick.borrow_mut() = Some(closure);
    core.schedule();
}

async fn probe_h264() -> Result<Option<(String, HardwareAcceleration)>, ApiError> {
    for codec in H264_CANDIDATES {
        for hardware in [
            HardwareAcceleration::PreferHardware,
            HardwareAcceleration::NoPreference,
        ] {
            let config = encoder_config_with(codec, REQUESTED_WIDTH, REQUESTED_HEIGHT, hardware);
            let promise = VideoEncoder::is_config_supported(&config);
            let support = JsFuture::from(promise).await.map_err(|value| {
                error::api_from_js(
                    &value,
                    error::CAPTURE_UNSUPPORTED,
                    "probing WebCodecs H.264 encoding",
                )
            })?;
            let supported = js_sys::Reflect::get(&support, &JsValue::from_str("supported"))
                .ok()
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            if supported {
                return Ok(Some((codec.to_string(), hardware)));
            }
        }
    }
    Ok(None)
}

fn encoder_config_with(
    codec: &str,
    width: u32,
    height: u32,
    hardware: HardwareAcceleration,
) -> VideoEncoderConfig {
    let config = VideoEncoderConfig::new(codec, height.max(2), width.max(2));
    config.set_width(width.max(2));
    config.set_height(height.max(2));
    config.set_bitrate(BITRATE);
    config.set_framerate(30.0);
    config.set_latency_mode(LatencyMode::Realtime);
    config.set_hardware_acceleration(hardware);
    // `avc` is a codec registration extension without a typed web-sys field.
    // Annex B is what the node's H.264 decoder accepts.
    let avc = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &avc,
        &JsValue::from_str("format"),
        &JsValue::from_str("annexb"),
    );
    let _ = js_sys::Reflect::set(config.as_ref(), &JsValue::from_str("avc"), &avc);
    config
}

fn layout_size(video: &HtmlVideoElement) -> (u32, u32) {
    let width = video.video_width();
    let height = video.video_height();
    if width == 0 || height == 0 {
        (REQUESTED_WIDTH, REQUESTED_HEIGHT)
    } else {
        (width, height)
    }
}

fn is_annexb(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 0, 1]) || data.starts_with(&[0, 0, 1])
}

fn prepare_video(
    document: &Document,
    stream: &MediaStream,
) -> Result<(HtmlVideoElement, bool), JsValue> {
    let (video, created) = match document
        .get_element_by_id("camera-preview")
        .and_then(|element| element.dyn_into::<HtmlVideoElement>().ok())
    {
        Some(video) => (video, false),
        None => {
            let element = document.create_element("video")?;
            let video = element.dyn_into::<HtmlVideoElement>()?;
            video.set_id("camera-preview");
            video.set_attribute("playsinline", "")?;
            video.set_attribute("aria-hidden", "true")?;
            let style = video.style();
            let _ = style.set_property("position", "absolute");
            let _ = style.set_property("left", "-10000px");
            let _ = style.set_property("top", "0");
            let _ = style.set_property("width", "1px");
            let _ = style.set_property("height", "1px");
            let _ = style.set_property("opacity", "0");
            let _ = style.set_property("pointer-events", "none");
            if let Some(body) = document.body() {
                body.append_child(&video)?;
            }
            (video, true)
        }
    };
    video.set_muted(true);
    video.set_autoplay(true);
    video.set_src_object(Some(stream));
    // A created element starts hidden; the demo's existing element keeps
    // whatever visibility the preview checkbox selected.
    if created {
        video.set_hidden(true);
    }
    Ok((video, created))
}

fn remove_preview(video: &HtmlVideoElement, created: bool) {
    if created {
        if let Some(parent) = video.parent_node() {
            let _ = parent.remove_child(video);
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

/// Releases a camera stream and temporary preview element on any early return
/// from `CameraPipeline::start_inner`.
struct CaptureGuard {
    stream: MediaStream,
    video: HtmlVideoElement,
    created: bool,
    armed: bool,
}

impl CaptureGuard {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        if self.armed {
            stop_tracks(&self.stream);
            let _ = self.video.pause();
            self.video.set_src_object(None);
            remove_preview(&self.video, self.created);
        }
    }
}
