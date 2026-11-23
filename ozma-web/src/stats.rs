// SPDX-License-Identifier: AGPL-3.0-only
//! Presentation and capture counters shared by the media loop, decoders,
//! canvas, WebCodecs encoders, and Web Audio.
//!
//! wasm32 is single-threaded, so plain `Cell`s are enough and avoid atomics on
//! the hot path.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsValue;

#[derive(Debug, Default)]
pub struct Stats {
    // Presentation (node -> browser).
    pub video_frames: Cell<u64>,
    pub video_dropped: Cell<u64>,
    pub video_errors: Cell<u64>,
    pub audio_frames: Cell<u64>,
    pub audio_dropped: Cell<u64>,
    pub audio_errors: Cell<u64>,
    pub bytes_in: Cell<u64>,
    pub width: Cell<u32>,
    pub height: Cell<u32>,

    // Uplink (browser -> node).
    pub camera_frames: Cell<u64>,
    pub camera_bytes: Cell<u64>,
    pub camera_dropped: Cell<u64>,
    pub camera_errors: Cell<u64>,
    pub camera_width: Cell<u32>,
    pub camera_height: Cell<u32>,
    pub mic_packets: Cell<u64>,
    pub mic_bytes: Cell<u64>,
    pub mic_errors: Cell<u64>,

    // Timing observation. Both clocks are `performance.now()` seconds so a
    // drift can be reported without claiming lip-sync. `audio_clock` is the
    // scheduled playout end of the most recent decoded buffer; `video_presented`
    // is when the canvas last drew a frame.
    pub audio_level: Cell<f64>,
    pub audio_clock: Cell<f64>,
    pub video_presented: Cell<f64>,
}

impl Stats {
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn add_bytes(&self, bytes: usize) {
        self.bytes_in
            .set(self.bytes_in.get().saturating_add(bytes as u64));
    }

    pub fn add_camera_bytes(&self, bytes: usize) {
        self.camera_bytes
            .set(self.camera_bytes.get().saturating_add(bytes as u64));
    }

    pub fn add_mic_bytes(&self, bytes: usize) {
        self.mic_bytes
            .set(self.mic_bytes.get().saturating_add(bytes as u64));
    }

    pub fn increment(cell: &Cell<u64>) {
        cell.set(cell.get().saturating_add(1));
    }

    pub fn to_js(&self) -> JsValue {
        let object = js_sys::Object::new();
        let set = |key: &str, value: JsValue| {
            let _ = js_sys::Reflect::set(&object, &JsValue::from_str(key), &value);
        };
        let number = |value: u64| JsValue::from_f64(value as f64);
        set("videoFrames", number(self.video_frames.get()));
        set("videoDropped", number(self.video_dropped.get()));
        set("videoErrors", number(self.video_errors.get()));
        set("audioFrames", number(self.audio_frames.get()));
        set("audioDropped", number(self.audio_dropped.get()));
        set("audioErrors", number(self.audio_errors.get()));
        set("width", JsValue::from_f64(self.width.get() as f64));
        set("height", JsValue::from_f64(self.height.get() as f64));
        set("bytesIn", number(self.bytes_in.get()));

        set("cameraFrames", number(self.camera_frames.get()));
        set("cameraBytes", number(self.camera_bytes.get()));
        set("cameraDropped", number(self.camera_dropped.get()));
        set("cameraErrors", number(self.camera_errors.get()));
        set(
            "cameraWidth",
            JsValue::from_f64(self.camera_width.get() as f64),
        );
        set(
            "cameraHeight",
            JsValue::from_f64(self.camera_height.get() as f64),
        );
        set("micPackets", number(self.mic_packets.get()));
        set("micBytes", number(self.mic_bytes.get()));
        set("micErrors", number(self.mic_errors.get()));

        let level = self.audio_level.get();
        set(
            "audioLevel",
            JsValue::from_f64(if level.is_finite() { level } else { 0.0 }),
        );

        let presented = self.video_presented.get();
        let clock = self.audio_clock.get();
        let drift_ms = if presented > 0.0 && clock > 0.0 {
            (presented - clock) * 1000.0
        } else {
            0.0
        };
        set(
            "avDriftMs",
            JsValue::from_f64(if drift_ms.is_finite() { drift_ms } else { 0.0 }),
        );

        object.into()
    }
}
