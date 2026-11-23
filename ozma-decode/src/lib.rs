// SPDX-License-Identifier: AGPL-3.0-only
//! Client-side decoding and frame conversion for Ozma.
//!
//! This crate is the receiving half of the M1 media path described in
//! `docs/node-hermes-parity.md` §8. The node captures and encodes H.264; a
//! client reassembles access units and needs to turn them back into pictures
//! and audio without a desktop toolkit or a hardware decoder.
//!
//! | Type | Role |
//! |---|---|
//! | [`VideoDecoder`] | object-safe decode boundary: `decode`, `flush`, `stop`, `format`, `stats` |
//! | [`H264Decoder`] | safe OpenH264 wrapper; Annex-B in, owned I420 [`DecodedVideoFrame`] out |
//! | [`DecodedVideoFrame`] | PTS/sequence/keyframe plus [`I420Planes`], `to_rgba`, `to_ppm`, `write_ppm` |
//! | [`AudioDecoder`] | thin wrapper over [`ozma_audio::OpusDecoder`] with PCM access |
//! | [`FrameBuffer`] | bounded FIFO of decoded frames with drop accounting |
//! | [`DecodeStats`] | frames, drops, errors |
//!
//! # Example
//!
//! ```no_run
//! use ozma_decode::{DecodeMeta, H264Decoder, VideoDecoder};
//!
//! # async fn show(access_unit: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
//! let mut decoder = H264Decoder::new()?;
//! if let Some(frame) = decoder
//!     .decode(access_unit, DecodeMeta::new(0, 0, true))
//!     .await?
//! {
//!     frame.write_ppm("/tmp/ozma-first-frame.ppm")?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Deliberate limits
//!
//! * **Annex-B only.** Length-prefixed (AVCC) access units are rejected as
//!   malformed. `ozma-encode` and H.264 pass-through already emit Annex-B.
//! * **Software decode.** OpenH264 is the decoder; there is no VAAPI, VideoToolbox,
//!   Media Foundation, or GPU import path here. That keeps `cargo test` working
//!   without hardware or a display.
//! * **8-bit I420/SDR.** P010/Main10/HDR and HEVC/AV1 are not decoded.
//! * **No colour VUI parsing.** [`DecodedVideoFrame::to_rgb`] uses the same
//!   BT.709-for-HD / BT.601-for-SD heuristic as the encoder, not the SPS VUI.
//! * **No GUI dependency.** The conversion helpers produce RGBA bytes or a PPM
//!   file; presenting those bytes belongs to the client application.
//! * **No jitter buffer or A/V sync.** Opus packets are decoded one at a time;
//!   ordering, concealment, and lip sync belong to the playback pipeline.
//!
//! The crate is `#![forbid(unsafe_code)]`; native work stays inside the
//! `openh264` and `audiopus` safe wrappers.

#![forbid(unsafe_code)]

pub mod audio;
pub mod buffer;
pub mod error;
pub mod frame;
pub mod h264;
pub mod stats;
pub mod video;

pub use audio::{AudioDecoder, DecodedAudio};
pub use buffer::FrameBuffer;
pub use error::{decode_codes, DecodeError};
pub use frame::{DecodeMeta, DecodedVideoFrame, I420Planes};
pub use h264::{H264Decoder, H264_DECODER_ID, MAX_PENDING_METADATA};
pub use stats::DecodeStats;
pub use video::{VideoDecoder, VideoFormat};
