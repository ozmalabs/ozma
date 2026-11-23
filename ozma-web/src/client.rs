// SPDX-License-Identifier: AGPL-3.0-only
//! The `#[wasm_bindgen]` client facade.
//!
//! `OzmaClient::connect` parses a `SessionOffer`, selects the WebTransport
//! descriptor, pins the node certificate by SHA-256, connects
//! [`BrowserTransport`], performs the core `Hello`/`HelloAck` handshake with
//! [`ClientDriver`], then hands both media decoders to a `spawn_local` loop.
//!
//! The facade is intentionally split:
//!
//! - the **media loop** owns the driver and the WebCodecs video decoder;
//! - the **input queue** is shared and drained by the media loop;
//! - the **stats** object is shared read-only with the page.
//!
//! `send_pointer` and friends are synchronous because DOM event handlers are
//! synchronous. They enqueue an [`InputEvent`]; the media loop sends queued
//! input before waiting for the next session event. That keeps one owner for
//! the driver's `&mut` half without cancelling a pending transport read (which
//! could lose reliable control bytes).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use ozma_client_core::session::{
    channel_map_for, webtransport_descriptor, ClientDriver, ClientEvent,
};
use ozma_protocol::{ChannelKind, ControlMessage, InputEvent, SessionOffer, TransportMode};
use ozma_transport::ChannelMap;
use ozma_transport_web::{BrowserConnectOptions, BrowserTransport};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlCanvasElement;

use crate::audio::AudioPipeline;
use crate::error;
use crate::input;
use crate::pin;
use crate::stats::Stats;
use crate::video::VideoPipeline;

/// Bound on queued input while the media loop is between events.
const MAX_QUEUED_INPUT: usize = 1024;

/// Shared state the media loop and the JS facade both touch.
struct Shared {
    stats: Rc<Stats>,
    inputs: RefCell<VecDeque<InputEvent>>,
    audio: RefCell<Option<AudioPipeline>>,
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

    fn enqueue(&self, event: InputEvent) -> Result<(), JsValue> {
        if self.closed.get() {
            return Err(error::client_error(
                error::DISCONNECTED,
                "The session is closed",
            ));
        }
        let mut inputs = self.inputs.borrow_mut();
        if inputs.len() >= MAX_QUEUED_INPUT {
            // Never grow without bound if a page fires synthetic events in a
            // tight loop. Keep the newest input, which is the useful one.
            inputs.pop_front();
        }
        inputs.push_back(event);
        Ok(())
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
            inputs: RefCell::new(VecDeque::new()),
            audio: RefCell::new(None),
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

    /// Queue absolute pointer motion in canvas/display pixels.
    pub fn send_pointer(&self, x: i32, y: i32) -> Result<(), JsValue> {
        self.shared.enqueue(input::pointer_absolute(x, y))
    }

    /// Queue a mouse button transition. `button` is the DOM `MouseEvent.button`
    /// index: 0 left, 1 middle, 2 right, 3 back, 4 forward.
    pub fn send_button(&self, button: u8, down: bool) -> Result<(), JsValue> {
        let event = input::button_event(button, down).ok_or_else(|| {
            error::client_error(
                error::INVALID_INPUT,
                format!("Unknown pointer button index {button}"),
            )
        })?;
        self.shared.enqueue(event)
    }

    /// Queue wheel scrolling in protocol wheel units.
    pub fn send_wheel(&self, dx: i32, dy: i32) -> Result<(), JsValue> {
        self.shared.enqueue(input::wheel_event(dx, dy))
    }

    /// Queue a key transition.
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
            .enqueue(input::key_event(hid_usage, down, modifiers))
    }

    /// Queue `ReleaseAll`. Sent on focus loss and disconnect; idempotent.
    pub fn release_all(&self) -> Result<(), JsValue> {
        self.shared.enqueue(InputEvent::ReleaseAll)
    }

    /// Request a keyframe.
    ///
    /// The current protocol has no client-to-node keyframe request message, so
    /// this reports `client.unsupported` instead of silently doing nothing.
    pub fn request_keyframe(&self) -> Result<(), JsValue> {
        Err(error::client_error(
            error::UNSUPPORTED,
            "The protocol has no client keyframe-request message yet",
        ))
    }

    /// Presentation counters.
    ///
    /// Returns `{videoFrames, videoDropped, videoErrors, audioFrames,
    /// audioDropped, audioErrors, width, height, bytesIn}`.
    pub fn stats(&self) -> JsValue {
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

    /// Ask the media loop to release input and close the transport.
    ///
    /// The close is performed by the loop, which owns the driver. It happens
    /// when the loop next observes an event or reaches the top of its wait, so
    /// a session that is actively streaming closes immediately; a completely
    /// idle transport is closed on the next event.
    pub fn close(&self) {
        if self.shared.closed.get() {
            return;
        }
        self.shared.close_requested.set(true);
        self.shared.notify("closing", "release all and close");
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
            if shared.close_requested.get() {
                let _ = driver.close().await;
                close_media(&video, &shared);
                shared.closed.set(true);
                shared.notify("closed", "client requested close");
                return;
            }

            // Drain input before blocking on the transport. The queue is only
            // filled by synchronous DOM handlers, so it is empty in the common
            // case. Take one event at a time so the queue is not borrowed while
            // the send future is pending.
            loop {
                let event = shared.inputs.borrow_mut().pop_front();
                let Some(event) = event else { break };
                if let Err(api) = driver.send_input(&event).await {
                    shared.notify_error(error::from_api_error(&api));
                }
            }

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

/// Release decoder and audio-device resources when the loop ends.
fn close_media(video: &VideoPipeline, shared: &Shared) {
    video.close();
    if let Some(audio) = shared.audio.borrow_mut().take() {
        audio.close();
    }
}
