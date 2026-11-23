// SPDX-License-Identifier: AGPL-3.0-only
//! Presentation counters shared by the media loop, decoders, and canvas.
//!
//! wasm32 is single-threaded, so plain `Cell`s are enough and avoid atomics on
//! the hot path.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsValue;

#[derive(Debug, Default)]
pub struct Stats {
    pub video_frames: Cell<u64>,
    pub video_dropped: Cell<u64>,
    pub video_errors: Cell<u64>,
    pub audio_frames: Cell<u64>,
    pub audio_dropped: Cell<u64>,
    pub audio_errors: Cell<u64>,
    pub bytes_in: Cell<u64>,
    pub width: Cell<u32>,
    pub height: Cell<u32>,
}

impl Stats {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn add_bytes(&self, bytes: usize) {
        self.bytes_in
            .set(self.bytes_in.get().saturating_add(bytes as u64));
    }

    pub fn to_js(&self) -> JsValue {
        let object = js_sys::Object::new();
        let set = |key: &str, value: JsValue| {
            let _ = js_sys::Reflect::set(&object, &JsValue::from_str(key), &value);
        };
        set(
            "videoFrames",
            JsValue::from_f64(self.video_frames.get() as f64),
        );
        set(
            "videoDropped",
            JsValue::from_f64(self.video_dropped.get() as f64),
        );
        set(
            "videoErrors",
            JsValue::from_f64(self.video_errors.get() as f64),
        );
        set(
            "audioFrames",
            JsValue::from_f64(self.audio_frames.get() as f64),
        );
        set(
            "audioDropped",
            JsValue::from_f64(self.audio_dropped.get() as f64),
        );
        set(
            "audioErrors",
            JsValue::from_f64(self.audio_errors.get() as f64),
        );
        set("width", JsValue::from_f64(self.width.get() as f64));
        set("height", JsValue::from_f64(self.height.get() as f64));
        set("bytesIn", JsValue::from_f64(self.bytes_in.get() as f64));
        object.into()
    }
}
