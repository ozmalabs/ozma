// SPDX-License-Identifier: AGPL-3.0-only
//! The `#[wasm_bindgen]` client facade.
//!
//! `OzmaClient::connect` parses a `SessionOffer`, selects the WebTransport
//! descriptor, pins the node certificate by SHA-256, connects
//! [`BrowserTransport`], performs the core `Hello`/`HelloAck` handshake with
//! [`ClientDriver`], then hands the driver and media decoders to a
//! `spawn_local` loop.
//!
//! The facade intentionally keeps the transport's cloneable [`BrowserSender`]
//! outside the media loop:
//!
//! - DOM input handlers serialize `ControlMessage::Input` and enqueue it on the
//!   reliable writer **synchronously**, so input is not delayed by a pending
//!   transport read.
//! - `close()` sends `ReleaseAll` and `Bye` through the same sender and then
//!   calls `BrowserSender::close`, which wakes a pending `recv` immediately.
//! - camera and microphone encoders send from their own `spawn_local` tasks;
//!   they never borrow the driver.
//!
//! The media loop therefore owns only the receive half: typed events, video
//! reassembly/decode, audio decode, and close observation.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use ozma_client_core::session::{
    channel_map_for, encode_control, webtransport_descriptor, ClientDriver, ClientEvent,
};
use ozma_protocol::{
    ChannelKind, ControlMessage, EndReason, InputEvent, SessionOffer, TransportMode,
};
use ozma_transport::{ChannelId, ChannelMap, CloseReason};
use ozma_transport_web::{BrowserConnectOptions, BrowserSender, BrowserTransport};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlCanvasElement;

use crate::audio::AudioPipeline;
use crate::camera::CameraPipeline;
use crate::capture::CaptureInfo;
use crate::error;
use crate::input;
use crate::mic::MicPipeline;
use crate::pin;
use crate::stats::Stats;
use crate::video::VideoPipeline;

/// Shared state the media loop and the JS facade both touch.
struct Shared {
    stats: Rc<Stats>,
    capture: Rc<CaptureInfo>,
    sender: RefCell<Option<BrowserSender>>,
    control_channel: Cell<Option<ChannelId>>,
    audio: RefCell<Option<AudioPipeline>>,
    camera: RefCell<Option<CameraPipeline>>,
    mic: RefCell<Option<MicPipeline>>,
    close_requested: Cell<bool>,
    closed: Cell<bool>,
    status: RefCell<String>,
    last_error: RefCell<Option<String>>,
    listener: RefCell<Option<js_sys::Function>>,
}

impl Shared {
    fn notify(&self, kind: &str, detail: &str) {
        *self.status.borrow_mut() = kind.to_string();
        let listener = self.listener.borrow().clone();
        if let Some(listener) = listener {
            let payload = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &payload,
                &JsValue::from_str("type"),
                &JsValue::from_str(kind),
            );
            let _ = js_sys::Reflect::set(
                &payload,
                &JsValue::from_str("detail"),
                &JsValue::from_str(detail),
            );
            let _ = listener.call1(&JsValue::NULL, &payload);
        }
    }

    fn notify_error(&self, value: JsValue) {
        let message = value
            .as_string()
            .or_else(|| {
                js_sys::Reflect::get(&value, &JsValue::from_str("message"))
                    .ok()
                    .and_then(|message| message.as_string())
            })
            .unwrap_or_else(|| format!("{value:?}"));
        *self.last_error.borrow_mut() = Some(message.clone());
        self.notify("error", &message);
    }

    fn sender_control(&self) -> Result<(BrowserSender, ChannelId), JsValue> {
        if self.closed.get() {
            return Err(error::client_error(
                error::DISCONNECTED,
                "The session is closed",
            ));
        }
        let sender = self.sender.borrow().clone().ok_or_else(|| {
            error::client_error(error::DISCONNECTED, "The session transport is not ready")
        })?;
        let control = self.control_channel.get().ok_or_else(|| {
            error::client_error(error::DISCONNECTED, "The session has no control channel")
        })?;
        Ok((sender, control))
    }

    /// Serialize and send one control message immediately.
    fn send_control(&self, message: ControlMessage) -> Result<(), JsValue> {
        let (sender, control) = self.sender_control()?;
        let bytes = encode_control(&message).map_err(|api| error::from_api_error(&api))?;
        send_now(&sender, control, Bytes::from(bytes))
    }

    fn send_input(&self, event: InputEvent) -> Result<(), JsValue> {
        self.send_control(ControlMessage::Input { event })
    }
}

/// A connected Ozma session in the browser.
#[wasm_bindgen]
pub struct OzmaClient {
    shared: Rc<Shared>,
}

#[wasm_bindgen]
impl OzmaClient {
    /// Connect to the node described by `offer_json`.
    ///
    /// - `offer_json` is the `offer` field from `POST /api/v1/sessions` or
    ///   `POST /api/v1/sessions/:id/start` (a serialized `SessionOffer`).
    /// - `certificate_der_base64` is the node's self-signed DER certificate,
    ///   from the additive `GET /api/v1/webtransport` endpoint (preferred) or
    ///   `GET /api/v1/transport`.
    ///
    /// The offer must contain a `quic` transport with an `https://` endpoint;
    /// that endpoint is the WebTransport URL. The certificate is hashed with
    /// SHA-256 and pinned for the handshake. A malformed offer, a missing
    /// WebTransport descriptor, or a certificate that does not match the
    /// offer's fingerprint produces a typed `OzmaError` instead of a panic.
    pub async fn connect(
        canvas: HtmlCanvasElement,
        offer_json: &str,
        certificate_der_base64: &str,
    ) -> Result<OzmaClient, JsValue> {
        console_error_panic_hook::set_once();

        let offer: SessionOffer = serde_json::from_str(offer_json).map_err(|parse| {
            error::client_error(
                error::INVALID_OFFER,
                format!("The session offer is not valid JSON: {parse}"),
            )
        })?;
        validate_offer(&offer)?;

        let descriptor =
            webtransport_descriptor(&offer).map_err(|api| error::from_api_error(&api))?;
        let url = descriptor
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                error::client_error(
                    error::INVALID_OFFER,
                    "The WebTransport descriptor has no endpoint",
                )
            })?
            .to_string();
        let channels = channel_map_for(descriptor).map_err(|api| error::from_api_error(&api))?;

        let certificate_sha256 = pin::certificate_sha256(
            certificate_der_base64,
            descriptor.certificate_fingerprint.as_deref(),
        )
        .map_err(|message| error::client_error(error::CERTIFICATE, message))?;

        // Build the presentation pipeline before the network handshake so a
        // browser without WebCodecs fails with a clear message.
        let stats = Stats::new();
        let video = VideoPipeline::new(canvas, stats.clone()).await?;

        let shared = Rc::new(Shared {
            stats: stats.clone(),
            capture: CaptureInfo::new(),
            sender: RefCell::new(None),
            control_channel: Cell::new(None),
            audio: RefCell::new(None),
            camera: RefCell::new(None),
            mic: RefCell::new(None),
            close_requested: Cell::new(false),
            closed: Cell::new(false),
            status: RefCell::new("connecting".to_string()),
            last_error: RefCell::new(None),
            listener: RefCell::new(None),
        });

        // Audio is best-effort: a browser without an Opus decoder should still
        // present video, and a suspended context is normal before a gesture.
        let audio = match AudioPipeline::new(stats.clone()) {
            Ok(audio) => Some(audio),
            Err(value) => {
                warn(&format!(
                    "audio pipeline unavailable; continuing with video only: {value:?}"
                ));
                None
            }
        };
        *shared.audio.borrow_mut() = audio;

        let max_payload = max_payload(&channels);
        let options = BrowserConnectOptions {
            url,
            certificate_sha256,
            channel_map: channels,
            max_payload,
        };
        let transport = BrowserTransport::connect(options)
            .await
            .map_err(|failure| {
                error::client_error(
                    error::TRANSPORT,
                    format!("WebTransport connection failed: {failure}"),
                )
            })?;
        let driver = ClientDriver::connect(transport, &offer)
            .await
            .map_err(|api| error::from_api_error(&api))?;

        // Keep the send half outside the media loop. Everything below this
        // point can enqueue input, capture control, and uplink media while the
        // media loop is waiting for the next transport event.
        let control = driver.channel_for(ChannelKind::Control).ok_or_else(|| {
            error::client_error(
                error::DISCONNECTED,
                "The negotiated channel map has no control channel",
            )
        })?;
        *shared.sender.borrow_mut() = Some(driver.transport().sender());
        shared.control_channel.set(Some(control));

        shared.notify(
            "connected",
            driver
                .session()
                .map(|session| format!("{:?}", session.state))
                .as_deref()
                .unwrap_or("unknown"),
        );

        let client = OzmaClient {
            shared: shared.clone(),
        };
        spawn_media_loop(driver, shared, video);
        Ok(client)
    }

    /// Send absolute pointer motion in canvas/display pixels immediately.
    pub fn send_pointer(&self, x: i32, y: i32) -> Result<(), JsValue> {
        self.shared.send_input(input::pointer_absolute(x, y))
    }

    /// Send a mouse button transition immediately. `button` is the DOM
    /// `MouseEvent.button` index: 0 left, 1 middle, 2 right, 3 back, 4 forward.
    pub fn send_button(&self, button: u8, down: bool) -> Result<(), JsValue> {
        let event = input::button_event(button, down).ok_or_else(|| {
            error::client_error(
                error::INVALID_INPUT,
                format!("Unknown pointer button index {button}"),
            )
        })?;
        self.shared.send_input(event)
    }

    /// Send wheel scrolling in protocol wheel units immediately.
    pub fn send_wheel(&self, dx: i32, dy: i32) -> Result<(), JsValue> {
        self.shared.send_input(input::wheel_event(dx, dy))
    }

    /// Send a key transition immediately.
    ///
    /// `hid_usage` is a USB HID keyboard usage id (page 0x07), **not** a
    /// JavaScript `keyCode`. `modifiers` is the HID boot-keyboard modifier byte
    /// (`1` ctrl, `2` shift, `4` alt, `8` meta, plus the right-hand high bits).
    /// `js_code_to_hid` translates `KeyboardEvent.code` in the demo.
    pub fn send_key(&self, hid_usage: u32, down: bool, modifiers: u8) -> Result<(), JsValue> {
        if hid_usage == 0 || hid_usage > 0xE7 {
            return Err(error::client_error(
                error::INVALID_INPUT,
                format!("{hid_usage} is not a USB HID keyboard usage id on page 0x07"),
            ));
        }
        self.shared
            .send_input(input::key_event(hid_usage, down, modifiers))
    }

    /// Send `ReleaseAll`. Sent on focus loss and disconnect; idempotent.
    pub fn release_all(&self) -> Result<(), JsValue> {
        self.shared.send_input(InputEvent::ReleaseAll)
    }

    /// Request a keyframe from the node's encoder.
    pub fn request_keyframe(&self) -> Result<(), JsValue> {
        self.shared.send_control(ControlMessage::KeyframeRequest {
            track: None,
        })
    }

    /// Start browser camera and/or microphone uplink.
    ///
    /// This must be awaited from a user gesture: it calls `getUserMedia` and
    /// resumes the microphone `AudioContext`. If one track fails the other is
    /// still started; the returned error describes the failed track and the
    /// node receives a `CaptureState` carrying the typed error.
    pub async fn start_capture(&self, video: bool, audio: bool) -> Result<(), JsValue> {
        let (sender, control) = self.shared.sender_control()?;
        let capture = self.shared.capture.clone();
        let stats = self.shared.stats.clone();
        capture.clear_error();

        send_control_async(
            &sender,
            control,
            ControlMessage::CaptureStart { video, audio },
        )
        .await?;

        let mut failure: Option<JsValue> = None;

        if video {
            let already = self.shared.camera.borrow().is_some();
            if !already {
                match CameraPipeline::start(sender.clone(), control, stats.clone(), capture.clone())
                    .await
                {
                    Ok(camera) => {
                        *self.shared.camera.borrow_mut() = Some(camera);
                    }
                    Err(value) => {
                        failure = Some(value);
                    }
                }
            }
        }
        if audio {
            let already = self.shared.mic.borrow().is_some();
            if !already {
                match MicPipeline::start(sender.clone(), control, stats, capture.clone()).await {
                    Ok(mic) => {
                        *self.shared.mic.borrow_mut() = Some(mic);
                    }
                    Err(value) => {
                        if failure.is_none() {
                            failure = Some(value);
                        }
                    }
                }
            }
        }

        // Report what is actually running, not what was requested.
        capture.set_video(self.shared.camera.borrow().is_some());
        capture.set_audio(self.shared.mic.borrow().is_some());
        capture.send_state(&sender, control);

        match failure {
            Some(value) => {
                self.shared.notify_error(value.clone());
                Err(value)
            }
            None => Ok(()),
        }
    }

    /// Stop camera and microphone uplink and report the stopped state.
    pub fn stop_capture(&self) -> Result<(), JsValue> {
        if self.shared.closed.get() {
            return Ok(());
        }
        let (sender, control) = self.shared.sender_control()?;
        let video = self.shared.camera.borrow_mut().take();
        let video_running = video.is_some();
        if let Some(camera) = video {
            camera.close();
        }
        let audio = self.shared.mic.borrow_mut().take();
        let audio_running = audio.is_some();
        if let Some(mic) = audio {
            mic.close();
        }
        self.shared.capture.set_video(false);
        self.shared.capture.set_audio(false);

        let message = ControlMessage::CaptureStop {
            video: video_running,
            audio: audio_running,
        };
        let bytes = encode_control(&message).map_err(|api| error::from_api_error(&api))?;
        send_now(&sender, control, Bytes::from(bytes))?;
        self.shared.capture.send_state(&sender, control);
        Ok(())
    }

    /// The current capture state:
    /// `{video, audio, videoCodec, width, height, error, errorCode}`.
    pub fn capture_state(&self) -> JsValue {
        self.shared.capture.to_js()
    }

    /// Presentation and capture counters.
    ///
    /// Returns a fresh object each call with the presentation counters
    /// (`videoFrames`, `videoDropped`, `videoErrors`, `audioFrames`,
    /// `audioDropped`, `audioErrors`, `width`, `height`, `bytesIn`), the uplink
    /// counters (`cameraFrames`, `cameraBytes`, `cameraDropped`,
    /// `cameraErrors`, `cameraWidth`, `cameraHeight`, `micPackets`, `micBytes`,
    /// `micErrors`), `audioLevel` (playback RMS, 0..1), and `avDriftMs`.
    pub fn stats(&self) -> JsValue {
        if let Some(audio) = self.shared.audio.borrow().as_ref() {
            self.shared.stats.audio_level.set(audio.level());
        }
        self.shared.stats.to_js()
    }

    /// The last status string (`connecting`, `connected`, `closed`, `error`).
    pub fn status(&self) -> String {
        self.shared.status.borrow().clone()
    }

    /// The last error message, if any.
    pub fn last_error(&self) -> Option<String> {
        self.shared.last_error.borrow().clone()
    }

    /// Register a callback for `{type, detail}` status notifications.
    pub fn set_listener(&self, listener: js_sys::Function) {
        *self.shared.listener.borrow_mut() = Some(listener);
    }

    /// Resume a suspended `AudioContext`. Browsers require a user gesture; call
    /// this from the first pointer or key handler.
    pub fn resume_audio(&self) -> Result<(), JsValue> {
        if let Some(audio) = self.shared.audio.borrow().as_ref() {
            audio.resume()?;
        }
        Ok(())
    }

    /// Send `ReleaseAll`, send `Bye`, and close the transport.
    ///
    /// All three happen in one spawned task through the cloneable sender, so an
    /// idle transport is closed immediately: `BrowserSender::close` enqueues a
    /// `Closed` event that wakes the media loop's pending `recv`.
    pub fn close(&self) {
        if self.shared.closed.get() || self.shared.close_requested.replace(true) {
            return;
        }
        self.shared.notify("closing", "release all and close");
        let shared = self.shared.clone();
        spawn_local(async move {
            if let Some(camera) = shared.camera.borrow_mut().take() {
                camera.close();
            }
            if let Some(mic) = shared.mic.borrow_mut().take() {
                mic.close();
            }
            shared.capture.set_video(false);
            shared.capture.set_audio(false);

            let sender = shared.sender.borrow().clone();
            let Some(sender) = sender else {
                shared.closed.set(true);
                shared.notify("closed", "no transport");
                return;
            };
            if let Some(control) = shared.control_channel.get() {
                if let Ok(bytes) = encode_control(&ControlMessage::Input {
                    event: InputEvent::ReleaseAll,
                }) {
                    let _ = sender.send(control, Bytes::from(bytes)).await;
                }
                if let Ok(bytes) = encode_control(&ControlMessage::Bye {
                    reason: EndReason::ClientDisconnected,
                }) {
                    let _ = sender.send(control, Bytes::from(bytes)).await;
                }
            }
            sender.close(CloseReason::Normal);
        });
    }
}

fn validate_offer(offer: &SessionOffer) -> Result<(), JsValue> {
    if offer.token.trim().is_empty() {
        return Err(error::client_error(
            error::INVALID_OFFER,
            "The session offer has an empty token",
        ));
    }
    if offer.transports.is_empty() {
        return Err(error::client_error(
            error::INVALID_OFFER,
            "The session offer has no transports",
        ));
    }
    let has_quic = offer
        .transports
        .iter()
        .any(|descriptor| descriptor.mode == TransportMode::Quic);
    if !has_quic {
        return Err(error::client_error(
            error::INVALID_OFFER,
            "The session offer has no quic/WebTransport transport",
        ));
    }
    Ok(())
}

fn max_payload(channels: &ChannelMap) -> usize {
    channels
        .iter()
        .map(|descriptor| descriptor.max_payload as usize)
        .max()
        .unwrap_or(1200)
        .max(1200)
}

/// Enqueue a payload on the transport synchronously when the adapter can
/// complete immediately.
///
/// Reliable channels (all DOM input and capture control) encode into the shared
/// `SendQueue` before the future's first `Poll::Ready`, so this preserves the
/// order of DOM events without awaiting the stream flush. A future that really
/// is pending (only possible on a datagram write) is handed to `spawn_local`
/// rather than dropped.
fn send_now(sender: &BrowserSender, channel: ChannelId, payload: Bytes) -> Result<(), JsValue> {
    let sender = sender.clone();
    let mut future = Box::pin(async move { sender.send(channel, payload).await });
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(Ok(())) => Ok(()),
        Poll::Ready(Err(failure)) => Err(error::client_error(
            error::TRANSPORT,
            format!("The transport rejected a send: {failure}"),
        )),
        Poll::Pending => {
            spawn_local(async move {
                let _ = future.await;
            });
            Ok(())
        }
    }
}

async fn send_control_async(
    sender: &BrowserSender,
    control: ChannelId,
    message: ControlMessage,
) -> Result<(), JsValue> {
    let bytes = encode_control(&message).map_err(|api| error::from_api_error(&api))?;
    sender
        .send(control, Bytes::from(bytes))
        .await
        .map_err(|failure| {
            error::client_error(
                error::TRANSPORT,
                format!("The transport rejected a control message: {failure}"),
            )
        })
}

/// Own the driver and decoders for the lifetime of the connection.
fn spawn_media_loop(
    driver: ClientDriver<BrowserTransport>,
    shared: Rc<Shared>,
    video: VideoPipeline,
) {
    spawn_local(async move {
        let mut driver = driver;
        let mut video = video;

        loop {
            match driver.next().await {
                ClientEvent::Media(packet) => {
                    shared.stats.add_bytes(packet.payload.len());
                    match packet.kind {
                        ChannelKind::Video => {
                            if let Err(value) =
                                video.push(packet.sequence, packet.keyframe, &packet.payload)
                            {
                                warn(&format!("video decode: {value:?}"));
                            }
                        }
                        ChannelKind::Audio => {
                            let mut audio = shared.audio.borrow_mut();
                            if let Some(audio) = audio.as_mut() {
                                if let Err(value) = audio.push(&packet.payload) {
                                    warn(&format!("audio decode: {value:?}"));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                ClientEvent::Control(message) => match message {
                    ControlMessage::HelloAck { session, .. } => {
                        shared.notify("session", &format!("{:?}", session.state));
                    }
                    ControlMessage::Event { event } => {
                        shared.notify("event", &format!("{event:?}"));
                    }
                    ControlMessage::Error { error } => {
                        shared.notify_error(error::from_api_error(&error));
                    }
                    ControlMessage::Bye { reason } => {
                        shared.notify("bye", &format!("{reason:?}"));
                    }
                    ControlMessage::KeyframeRequest { .. } => {
                        // The node can ask the browser publisher for an IDR.
                        if let Some(camera) = shared.camera.borrow().as_ref() {
                            camera.request_keyframe();
                        }
                        shared.notify("keyframe_request", "forcing a camera keyframe");
                    }
                    _ => {}
                },
                ClientEvent::Error(error) => {
                    shared.notify_error(error::from_api_error(&error));
                }
                ClientEvent::Closed(reason) => {
                    close_media(&video, &shared);
                    shared.closed.set(true);
                    shared.notify("closed", &format!("{reason:?}"));
                    return;
                }
            }
        }
    });
}

fn warn(message: &str) {
    web_sys::console::warn_1(&JsValue::from_str(message));
}

/// Release decoder, encoder, and audio-device resources when the session ends.
fn close_media(video: &VideoPipeline, shared: &Shared) {
    video.close();
    if let Some(audio) = shared.audio.borrow_mut().take() {
        audio.close();
    }
    if let Some(camera) = shared.camera.borrow_mut().take() {
        camera.close();
    }
    if let Some(mic) = shared.mic.borrow_mut().take() {
        mic.close();
    }
    shared.capture.set_video(false);
    shared.capture.set_audio(false);
}
