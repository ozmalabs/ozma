// SPDX-License-Identifier: AGPL-3.0-only
//! Shared browser capture bookkeeping and `ControlMessage::CaptureState`
//! reporting.
//!
//! The camera and microphone pipelines each own their browser objects, but both
//! update this small cell bundle so the facade can answer `capture_state()` and
//! report one consistent state to the node.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use bytes::Bytes;
use ozma_client_core::encode_control;
use ozma_protocol::{ApiError, ControlMessage};
use ozma_transport::ChannelId;
use ozma_transport_web::BrowserSender;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::spawn_local;

#[derive(Debug, Default)]
pub struct CaptureInfo {
    video: Cell<bool>,
    audio: Cell<bool>,
    codec: RefCell<Option<String>>,
    width: Cell<u32>,
    height: Cell<u32>,
    error: RefCell<Option<ApiError>>,
}

impl CaptureInfo {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn set_video(&self, video: bool) {
        self.video.set(video);
    }

    pub fn set_audio(&self, audio: bool) {
        self.audio.set(audio);
    }

    pub fn set_codec(&self, codec: &str) {
        *self.codec.borrow_mut() = Some(codec.to_string());
    }

    pub fn set_size(&self, width: u32, height: u32) {
        self.width.set(width);
        self.height.set(height);
    }

    pub fn set_error(&self, error: ApiError) {
        *self.error.borrow_mut() = Some(error);
    }

    pub fn clear_error(&self) {
        *self.error.borrow_mut() = None;
    }

    /// The JS shape returned by `OzmaClient.capture_state()`.
    ///
    /// `error` is the human-readable message (or `null`) and `errorCode` the
    /// stable protocol code, so the demo can display both without parsing.
    pub fn to_js(&self) -> JsValue {
        let object = js_sys::Object::new();
        let set = |key: &str, value: JsValue| {
            let _ = js_sys::Reflect::set(&object, &JsValue::from_str(key), &value);
        };
        set("video", JsValue::from_bool(self.video.get()));
        set("audio", JsValue::from_bool(self.audio.get()));
        let codec = self.codec.borrow().clone();
        set(
            "videoCodec",
            match codec {
                Some(codec) => JsValue::from_str(&codec),
                None => JsValue::NULL,
            },
        );
        set("width", JsValue::from_f64(self.width.get() as f64));
        set("height", JsValue::from_f64(self.height.get() as f64));
        let error = self.error.borrow().clone();
        match error {
            Some(error) => {
                set("error", JsValue::from_str(&error.message));
                set("errorCode", JsValue::from_str(&error.code));
            }
            None => {
                set("error", JsValue::NULL);
                set("errorCode", JsValue::NULL);
            }
        }
        object.into()
    }

    /// Encode and send `CaptureState` with the real codec, dimensions, and
    /// optional error. Sends are spawned so a synchronous caller (for example
    /// `stop_capture`) never blocks the DOM event.
    pub fn send_state(&self, sender: &BrowserSender, control: ChannelId) {
        let width = self.width.get();
        let height = self.height.get();
        let message = ControlMessage::CaptureState {
            video: self.video.get(),
            audio: self.audio.get(),
            video_codec: self.codec.borrow().clone(),
            width: (width > 0 && width <= u32::from(u16::MAX)).then_some(width as u16),
            height: (height > 0 && height <= u32::from(u16::MAX)).then_some(height as u16),
            error: self.error.borrow().clone(),
        };
        let Ok(bytes) = encode_control(&message) else {
            return;
        };
        let sender = sender.clone();
        spawn_local(async move {
            let _ = sender.send(control, Bytes::from(bytes)).await;
        });
    }
}
