// SPDX-License-Identifier: AGPL-3.0-only
//! A safe OpenH264-backed H.264 decoder.
//!
//! [`H264Decoder`] consumes the Annex-B access units produced by
//! `ozma-encode` and by H.264 pass-through capture, and returns owned
//! [`DecodedVideoFrame`] values. It uses the safe `openh264` wrapper; this crate
//! stays `#![forbid(unsafe_code)]` and never touches the native pointers the
//! wrapper intentionally hides.
//!
//! # Reordering and `None`
//!
//! The decoder is configured with OpenH264's per-call flush disabled so a call
//! only returns a picture produced by the access unit it was given. In the
//! Ozma low-latency profile (Baseline, no B-frames) that is one picture per
//! non-header access unit.
//!
//! The wrapper can still return `Ok(None)` in two normal cases:
//!
//! * the access unit contained only SPS/PPS/SEI (or was otherwise not a
//!   picture) — no output is expected;
//! * the backend is holding a picture for reordering.
//!
//! Metadata for pictures is therefore queued when an access unit contains a
//! VCL NAL unit (types 1–5), and output is attributed to the oldest outstanding
//! unit. [`VideoDecoder::flush`] drains pictures the backend still holds. The
//! queue is bounded; if more than [`MAX_PENDING_METADATA`] pictures are
//! outstanding, the oldest metadata is dropped and counted as a drop rather
//! than silently mislabelling a later frame.
//!
//! # Input validation
//!
//! Empty access units, payloads without an Annex-B start code, start codes with
//! no NAL header, the reserved NAL type 0, and NAL units with the forbidden bit
//! set are rejected before they reach OpenH264. That gives the transport a
//! deterministic structured error instead of a backend-specific message. A
//! structurally valid but truncated bitstream is still reported by OpenH264 as
//! [`crate::DecodeError::Malformed`].

use std::collections::VecDeque;

use async_trait::async_trait;
use openh264::decoder::{DecodedYUV, Decoder, DecoderConfig, Flush};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use ozma_protocol::Codec;

use crate::error::DecodeError;
use crate::frame::{DecodeMeta, DecodedVideoFrame, I420Planes};
use crate::stats::DecodeStats;
use crate::video::{VideoDecoder, VideoFormat};

/// Stable identifier for this decoder.
pub const H264_DECODER_ID: &str = "openh264-h264";

/// Maximum number of access-unit metadata entries retained for reordering.
///
/// A low-latency H.264 stream has one outstanding picture at most. The bound
/// exists so a broken or unexpected stream cannot grow client memory without
/// limit.
pub const MAX_PENDING_METADATA: usize = 16;

/// A safe OpenH264 H.264 decoder for client playback and headless checks.
pub struct H264Decoder {
    id: String,
    decoder: Option<Decoder>,
    width: Option<u32>,
    height: Option<u32>,
    pending_meta: VecDeque<DecodeMeta>,
    last_meta: Option<DecodeMeta>,
    stats: DecodeStats,
}

impl H264Decoder {
    /// Create a decoder using the bundled OpenH264 sources.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::Unavailable`] when OpenH264 cannot be initialised
    /// on this target. No decoder is constructed and no frame is ever faked.
    pub fn new() -> Result<Self, DecodeError> {
        // `Flush::NoFlush` keeps decode calls attributable: a picture returned
        // by `decode` was produced by the access unit just supplied. Frames the
        // backend holds for reordering are drained explicitly by `flush`.
        let config = DecoderConfig::new().flush_after_decode(Flush::NoFlush);
        let decoder =
            Decoder::with_api_config(OpenH264API::from_source(), config).map_err(|error| {
                DecodeError::unavailable(format!("OpenH264 decoder initialisation failed: {error}"))
            })?;
        Ok(Self {
            id: H264_DECODER_ID.to_owned(),
            decoder: Some(decoder),
            width: None,
            height: None,
            pending_meta: VecDeque::new(),
            last_meta: None,
            stats: DecodeStats::new(),
        })
    }

    /// True until [`VideoDecoder::stop`] has been called.
    pub fn is_open(&self) -> bool {
        self.decoder.is_some()
    }

    /// Number of access-unit metadata entries waiting for backend output.
    ///
    /// This is normally `0` after every decoded frame in the low-latency
    /// profile. A non-zero value after a frame indicates that the backend held
    /// a picture and the next call or [`VideoDecoder::flush`] should drain it.
    pub fn pending_metadata(&self) -> usize {
        self.pending_meta.len()
    }
}

#[async_trait]
impl VideoDecoder for H264Decoder {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> VideoFormat {
        VideoFormat::new(
            Codec::H264,
            self.width.unwrap_or(0),
            self.height.unwrap_or(0),
        )
    }

    fn stats(&self) -> DecodeStats {
        self.stats
    }

    async fn decode(
        &mut self,
        access_unit: &[u8],
        meta: DecodeMeta,
    ) -> Result<Option<DecodedVideoFrame>, DecodeError> {
        if self.decoder.is_none() {
            return Err(DecodeError::closed(&self.id));
        }

        let nal_types = match annex_b_nal_types(access_unit) {
            Ok(types) => types,
            Err(error) => {
                self.stats.record_error();
                return Err(error);
            }
        };
        let has_picture = nal_types.iter().any(|nal| (1..=5).contains(nal));

        if has_picture {
            if self.pending_meta.len() >= MAX_PENDING_METADATA {
                self.pending_meta.pop_front();
                self.stats.record_drop();
            }
            self.pending_meta.push_back(meta);
        }
        self.last_meta = Some(meta);

        let decoded = {
            let decoder = self.decoder.as_mut().expect("checked above");
            match decoder.decode(access_unit) {
                Ok(Some(yuv)) => {
                    // FIFO attribution: in order this is exactly `meta`; if the
                    // backend held earlier pictures, this is the oldest one.
                    let frame_meta = if has_picture {
                        self.pending_meta.pop_front().unwrap_or(meta)
                    } else {
                        meta
                    };
                    own_frame(&yuv, frame_meta).map(Some)
                }
                Ok(None) => Ok(None),
                Err(error) => {
                    // The failed unit must not leave a phantom entry that
                    // shifts every later timestamp.
                    if has_picture && self.pending_meta.back() == Some(&meta) {
                        self.pending_meta.pop_back();
                    }
                    Err(DecodeError::malformed(format!(
                        "OpenH264 rejected the access unit: {error}"
                    )))
                }
            }
        };

        match decoded {
            Ok(Some(frame)) => {
                self.width = Some(frame.width);
                self.height = Some(frame.height);
                self.stats.record_frame();
                Ok(Some(frame))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                self.stats.record_error();
                Err(error)
            }
        }
    }

    async fn flush(&mut self) -> Result<Vec<DecodedVideoFrame>, DecodeError> {
        if self.decoder.is_none() {
            return Err(DecodeError::closed(&self.id));
        }

        let (frames, error) = {
            let decoder = self.decoder.as_mut().expect("checked above");
            match decoder.flush_remaining() {
                Ok(yuvs) => {
                    let mut frames = Vec::with_capacity(yuvs.len());
                    let mut error = None;
                    for yuv in yuvs {
                        let meta = self
                            .pending_meta
                            .pop_front()
                            .or(self.last_meta)
                            .unwrap_or_default();
                        match own_frame(&yuv, meta) {
                            Ok(frame) => frames.push(frame),
                            Err(frame_error) => {
                                error = Some(frame_error);
                                break;
                            }
                        }
                    }
                    (frames, error)
                }
                Err(flush_error) => (
                    Vec::new(),
                    Some(DecodeError::backend(format!(
                        "OpenH264 failed to flush delayed pictures: {flush_error}"
                    ))),
                ),
            }
        };

        if let Some(error) = error {
            self.stats.record_error();
            return Err(error);
        }

        for frame in &frames {
            self.width = Some(frame.width);
            self.height = Some(frame.height);
            self.stats.record_frame();
        }
        Ok(frames)
    }

    async fn stop(&mut self) -> Result<(), DecodeError> {
        // Dropping the OpenH264 decoder runs the native uninitialise step.
        self.decoder = None;
        self.pending_meta.clear();
        self.last_meta = None;
        Ok(())
    }
}

/// NAL unit types in an Annex-B buffer, after structural validation.
///
/// Valid H.264 escapes emulation patterns (`00 00 03`), so a `00 00 01` or
/// `00 00 00 01` sequence is a start code. This deliberately does not parse
/// SPS/PPS payloads; its job is to reject garbage before invoking the backend.
fn annex_b_nal_types(data: &[u8]) -> Result<Vec<u8>, DecodeError> {
    if data.is_empty() {
        return Err(DecodeError::EmptyAccessUnit);
    }

    let mut types = Vec::new();
    let mut index = 0;
    let mut saw_start_code = false;

    while index + 3 <= data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        saw_start_code = true;

        let header_index = index + start_len;
        let Some(header) = data.get(header_index) else {
            return Err(DecodeError::malformed(format!(
                "start code at byte {index} has no NAL header"
            )));
        };
        if header & 0x80 != 0 {
            return Err(DecodeError::malformed(format!(
                "NAL unit at byte {header_index} has the forbidden_zero_bit set"
            )));
        }
        let nal_type = header & 0x1f;
        if nal_type == 0 {
            return Err(DecodeError::malformed(format!(
                "NAL unit at byte {header_index} uses the reserved type 0"
            )));
        }
        types.push(nal_type);

        // Continue after this header. Start codes only occur between NAL
        // units, so there is no need to rescan payload bytes.
        index = header_index + 1;
    }

    if !saw_start_code {
        return Err(DecodeError::malformed(
            "no Annex-B start code (00 00 01 or 00 00 00 01) found",
        ));
    }
    if types.is_empty() {
        return Err(DecodeError::malformed("no NAL units found"));
    }
    Ok(types)
}

/// Copy one strided plane from OpenH264 into tightly packed rows.
fn copy_plane(
    plane: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Result<Vec<u8>, DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::invalid_frame(
            "decoder returned a plane with zero width or height",
        ));
    }
    if stride < width {
        return Err(DecodeError::invalid_frame(format!(
            "decoder plane stride {stride} is smaller than width {width}"
        )));
    }
    let needed = stride
        .checked_mul(height)
        .ok_or_else(|| DecodeError::invalid_frame("decoder plane size overflows usize"))?;
    if plane.len() < needed {
        return Err(DecodeError::invalid_frame(format!(
            "decoder plane is {} bytes but a {stride}-byte stride over {height} rows needs {needed}",
            plane.len()
        )));
    }
    let capacity = width
        .checked_mul(height)
        .ok_or_else(|| DecodeError::invalid_frame("decoded frame size overflows usize"))?;
    let mut packed = Vec::with_capacity(capacity);
    for row in 0..height {
        let start = row * stride;
        packed.extend_from_slice(&plane[start..start + width]);
    }
    Ok(packed)
}

/// Copy a decoded OpenH264 picture into an owned [`DecodedVideoFrame`].
fn own_frame(yuv: &DecodedYUV<'_>, meta: DecodeMeta) -> Result<DecodedVideoFrame, DecodeError> {
    let (width, height) = yuv.dimensions();
    if width == 0 || height == 0 {
        return Err(DecodeError::invalid_frame(format!(
            "decoder returned a zero-sized picture ({width}x{height})"
        )));
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err(DecodeError::invalid_frame(format!(
            "decoder returned odd dimensions {width}x{height}; I420 output requires even dimensions"
        )));
    }

    let (y_stride, u_stride, v_stride) = yuv.strides();
    let luma = copy_plane(yuv.y(), y_stride, width, height)?;
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let u = copy_plane(yuv.u(), u_stride, chroma_width, chroma_height)?;
    let v = copy_plane(yuv.v(), v_stride, chroma_width, chroma_height)?;

    let width = u32::try_from(width)
        .map_err(|_| DecodeError::invalid_frame("decoded width does not fit u32"))?;
    let height = u32::try_from(height)
        .map_err(|_| DecodeError::invalid_frame("decoded height does not fit u32"))?;

    DecodedVideoFrame::new(meta, width, height, I420Planes::new(luma, u, v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_access_units_are_rejected_before_the_backend() {
        assert_eq!(
            annex_b_nal_types(&[]).unwrap_err(),
            DecodeError::EmptyAccessUnit
        );
    }

    #[test]
    fn payloads_without_a_start_code_are_malformed() {
        let error = annex_b_nal_types(b"definitely not h264").unwrap_err();
        assert_eq!(error.code(), crate::decode_codes::MALFORMED_ACCESS_UNIT);
        assert!(error.to_string().contains("start code"));
    }

    #[test]
    fn a_start_code_without_a_header_is_malformed() {
        let error = annex_b_nal_types(&[0, 0, 1]).unwrap_err();
        assert_eq!(error.code(), crate::decode_codes::MALFORMED_ACCESS_UNIT);
        assert!(error.to_string().contains("NAL header"));
    }

    #[test]
    fn reserved_and_forbidden_nal_headers_are_rejected() {
        assert!(annex_b_nal_types(&[0, 0, 1, 0x00]).is_err());
        assert!(annex_b_nal_types(&[0, 0, 1, 0x80]).is_err());
    }

    #[test]
    fn sps_pps_and_idr_types_are_found() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 1, 0x68, 3, // PPS
            0, 0, 1, 0x65, 4, // IDR
        ];
        assert_eq!(annex_b_nal_types(&data).unwrap(), vec![7, 8, 5]);
    }

    #[test]
    fn copy_plane_removes_stride_padding() {
        // Two rows with a 4-byte stride but only 3 visible columns.
        let data = [1, 2, 3, 99, 4, 5, 6, 99];
        assert_eq!(copy_plane(&data, 4, 3, 2).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn copy_plane_rejects_short_input() {
        assert!(copy_plane(&[1, 2, 3], 4, 3, 2).is_err());
    }
}
