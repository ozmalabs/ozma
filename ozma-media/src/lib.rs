// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-media` — frames, capture/encode boundaries, and a deterministic
//! pipeline for CI.
//!
//! The crate deliberately has no platform dependencies. Real capture lives in
//! `ozma-capture` and real encoders in `ozma-encode`; both implement the traits
//! defined here. `FakeCapture`, `FakeEncoder`, and [`Pipeline`] are the tested
//! path used by node and session tests.
//!
//! ```no_run
//! use ozma_media::{FakeCapture, FakeEncoder, FrameFormat, Pipeline, PipelineConfig};
//!
//! let source = FakeCapture::default().with_frame_limit(1).with_fps(1_000);
//! let encoder = FakeEncoder::h264(FrameFormat::rgb24(320, 240), 2_000);
//! let mut pipeline = Pipeline::start(source, encoder, PipelineConfig::default()).unwrap();
//! # let _ = &mut pipeline;
//! ```
//!
//! The example above starts a pipeline and drops it immediately; use
//! `RunningPipeline::recv` and `RunningPipeline::wait` in real code.

#![forbid(unsafe_code)]

pub mod cancel;
pub mod error;
pub mod fake;
pub mod frame;
pub mod pipeline;
pub mod traits;

pub use cancel::CancelToken;
pub use error::{media_codes, MediaError};
pub use fake::{FakeCapture, FakeEncoder};
pub use frame::{EncodedFrame, Frame, FrameBuffer, FrameFormat, PixelFormat};
pub use pipeline::{
    Pipeline, PipelineConfig, PipelineMetrics, RunningPipeline, TrackPipeline,
};
pub use traits::{CaptureSource, Encoder};

/// Re-export of the protocol media vocabulary most callers need alongside
/// this crate's frame types.
pub use ozma_protocol::{Codec, ColorFormat, MediaFormat};
