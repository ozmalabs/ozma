// SPDX-License-Identifier: AGPL-3.0-only
//! Canvas presentation.
//!
//! Decoded [`VideoFrame`]s are drawn with the 2D context's typed `VideoFrame`
//! overload. The canvas backing size follows the decoded frame size; CSS scales
//! the element to the page, so a 4K stream does not silently become a 1080p
//! backing store.

use std::rc::Rc;

use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, VideoFrame};

use crate::error;
use crate::stats::Stats;

pub struct CanvasSurface {
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
    stats: Rc<Stats>,
    last_size: Option<(u32, u32)>,
}

impl CanvasSurface {
    pub fn new(canvas: HtmlCanvasElement, stats: Rc<Stats>) -> Result<Self, wasm_bindgen::JsValue> {
        let context = canvas
            .get_context("2d")
            .map_err(|error| crate::error::from_js(error, error::UNSUPPORTED, "2D canvas"))?
            .ok_or_else(|| {
                crate::error::client_error(
                    error::UNSUPPORTED,
                    "The canvas has no 2D rendering context",
                )
            })?
            .dyn_into::<CanvasRenderingContext2d>()
            .map_err(|_| {
                crate::error::client_error(
                    error::UNSUPPORTED,
                    "The canvas context is not a CanvasRenderingContext2d",
                )
            })?;
        Ok(Self {
            canvas,
            context,
            stats,
            last_size: None,
        })
    }

    /// Draw one decoded frame and consume it.
    ///
    /// The first frame (and any size change) fixes the canvas backing store to
    /// the frame's display size, then the frame is stretched to that size. A
    /// zero-sized frame is dropped rather than drawn.
    pub fn draw(&mut self, frame: VideoFrame) {
        let width = frame.display_width();
        let height = frame.display_height();
        if width == 0 || height == 0 {
            frame.close();
            self.stats
                .video_dropped
                .set(self.stats.video_dropped.get() + 1);
            return;
        }
        if self.last_size != Some((width, height)) {
            self.canvas.set_width(width);
            self.canvas.set_height(height);
            self.stats.width.set(width);
            self.stats.height.set(height);
            self.last_size = Some((width, height));
        }
        let target_width = self.canvas.width().max(1) as f64;
        let target_height = self.canvas.height().max(1) as f64;
        let _ = self.context.draw_image_with_video_frame_and_dw_and_dh(
            &frame,
            0.0,
            0.0,
            target_width,
            target_height,
        );
        frame.close();
        self.stats
            .video_frames
            .set(self.stats.video_frames.get() + 1);
    }
}
