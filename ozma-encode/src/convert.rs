// SPDX-License-Identifier: AGPL-3.0-only
//! Capture-layout conversion to packed RGB24.
//!
//! V4L2 devices commonly negotiate `MJPG`, `NV12`, or `YUYV`, while the local
//! HLS feeder writes `rawvideo` RGB24 to ffmpeg. This module provides one
//! bounded, explicit conversion path for those layouts so the pipeline never
//! reinterprets compressed or YUV bytes as RGB.
//!
//! Only layouts this crate can decode without guessing are accepted. A short
//! buffer, odd chroma dimensions, or a decoded JPEG whose geometry disagrees
//! with the negotiated capture format is an error rather than a silently
//! corrupt frame.

use ozma_media::{FrameFormat, PixelFormat};

/// One capture frame could not be converted to RGB24.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// The layout has no RGB24 conversion in this crate.
    #[error("cannot convert `{0}` frames to rgb24")]
    UnsupportedFormat(PixelFormat),
    /// Width or height is zero.
    #[error("frame dimensions must be non-zero, got {width}x{height}")]
    ZeroDimensions {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
    /// A YUV layout requires even chroma dimensions.
    #[error("{format} requires even dimensions, got {width}x{height}")]
    OddDimensions {
        /// Layout name.
        format: PixelFormat,
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
    /// The payload is shorter than the layout requires.
    #[error(
        "frame buffer is {actual} bytes, expected at least {expected} for {format} at {width}x{height}"
    )]
    FrameTooShort {
        /// Declared frame layout.
        format: PixelFormat,
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Minimum required byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
    /// A decoded MJPEG frame had a different size than the capture format.
    #[error("decoded MJPEG image is {decoded_width}x{decoded_height}, expected {width}x{height}")]
    DecodedDimensions {
        /// Negotiated frame width.
        width: u32,
        /// Negotiated frame height.
        height: u32,
        /// Decoded image width.
        decoded_width: u32,
        /// Decoded image height.
        decoded_height: u32,
    },
    /// The compressed payload could not be decoded.
    #[error("MJPEG decode failed: {0}")]
    Decode(String),
    /// The nominal length overflows the host size.
    #[error("frame dimensions {width}x{height} overflow a host buffer")]
    Overflow {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
}

/// Convert a captured frame payload to tightly packed RGB24.
pub fn to_rgb24(format: FrameFormat, data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let width = format.width;
    let height = format.height;
    if width == 0 || height == 0 {
        return Err(ConvertError::ZeroDimensions { width, height });
    }
    let rgb_len = checked_len(width, height, 3)?;

    match format.pixel_format {
        PixelFormat::Rgb24 => {
            require_len(format, data, rgb_len)?;
            Ok(data[..rgb_len].to_vec())
        }
        PixelFormat::Bgra => {
            let needed = checked_len(width, height, 4)?;
            require_len(format, data, needed)?;
            let mut rgb = Vec::with_capacity(rgb_len);
            for pixel in data[..needed].chunks_exact(4) {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
            Ok(rgb)
        }
        PixelFormat::Yuyv => yuyv_to_rgb24(width, height, data),
        PixelFormat::Nv12 => nv12_to_rgb24(width, height, data),
        PixelFormat::Mjpeg => decode_mjpeg_to_rgb24(width, height, data),
        other => Err(ConvertError::UnsupportedFormat(other)),
    }
}

fn yuyv_to_rgb24(width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    if width % 2 != 0 || height % 2 != 0 {
        return Err(ConvertError::OddDimensions {
            format: PixelFormat::Yuyv,
            width,
            height,
        });
    }
    let needed = checked_len(width, height, 2)?;
    require_len(
        FrameFormat::new(PixelFormat::Yuyv, width, height),
        data,
        needed,
    )?;

    let mut rgb = Vec::with_capacity(checked_len(width, height, 3)?);
    for pair in data[..needed].chunks_exact(4) {
        let y0 = i32::from(pair[0]);
        let u = i32::from(pair[1]);
        let y1 = i32::from(pair[2]);
        let v = i32::from(pair[3]);
        push_yuv_pixel(&mut rgb, y0, u, v);
        push_yuv_pixel(&mut rgb, y1, u, v);
    }
    Ok(rgb)
}

fn nv12_to_rgb24(width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    if width % 2 != 0 || height % 2 != 0 {
        return Err(ConvertError::OddDimensions {
            format: PixelFormat::Nv12,
            width,
            height,
        });
    }
    let luma_len = checked_len(width, height, 1)?;
    let needed = luma_len + checked_len(width / 2, height / 2, 2)?;
    require_len(
        FrameFormat::new(PixelFormat::Nv12, width, height),
        data,
        needed,
    )?;

    let stride = width as usize;
    let chroma_stride = (width / 2) as usize;
    let mut rgb = Vec::with_capacity(checked_len(width, height, 3)?);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let luma = data[y * stride + x];
            let chroma_index = luma_len + (y / 2) * chroma_stride * 2 + (x / 2) * 2;
            let u = data[chroma_index];
            let v = data[chroma_index + 1];
            push_yuv_pixel(&mut rgb, i32::from(luma), i32::from(u), i32::from(v));
        }
    }
    Ok(rgb)
}

fn decode_mjpeg_to_rgb24(width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    use jpeg_decoder::{Decoder, PixelFormat as JpegPixelFormat};

    let mut decoder = Decoder::new(std::io::Cursor::new(data));
    let pixel_limit = checked_len(width, height, 4)?.max(1 << 20);
    decoder.set_max_decoding_buffer_size(pixel_limit);
    let pixels = decoder
        .decode()
        .map_err(|error| ConvertError::Decode(format!("malformed MJPEG frame: {error}")))?;
    let info = decoder.info().ok_or_else(|| {
        ConvertError::Decode("malformed MJPEG frame: no image information".to_owned())
    })?;

    let decoded_width = u32::from(info.width);
    let decoded_height = u32::from(info.height);
    if decoded_width != width || decoded_height != height {
        return Err(ConvertError::DecodedDimensions {
            width,
            height,
            decoded_width,
            decoded_height,
        });
    }

    match info.pixel_format {
        JpegPixelFormat::RGB24 => {
            let expected = checked_len(width, height, 3)?;
            require_len(
                FrameFormat::new(PixelFormat::Mjpeg, width, height),
                &pixels,
                expected,
            )?;
            Ok(pixels)
        }
        JpegPixelFormat::L8 => {
            let expected = checked_len(width, height, 1)?;
            require_len(
                FrameFormat::new(PixelFormat::Mjpeg, width, height),
                &pixels,
                expected,
            )?;
            let mut rgb = Vec::with_capacity(checked_len(width, height, 3)?);
            for value in pixels {
                rgb.extend_from_slice(&[value, value, value]);
            }
            Ok(rgb)
        }
        other => Err(ConvertError::Decode(format!(
            "unsupported MJPEG pixel format {other:?}; only 8-bit RGB and grayscale are converted"
        ))),
    }
}

#[inline]
fn push_yuv_pixel(rgb: &mut Vec<u8>, y: i32, u: i32, v: i32) {
    // Limited-range BT.601, matching the YUYV/NV12 assumption used by the
    // software encoder. The source is a capture device, not full-range JPEG.
    let c = (y - 16).max(0);
    let d = u - 128;
    let e = v - 128;
    let r = ((298 * c + 409 * e + 128) >> 8).clamp(0, 255);
    let g = ((298 * c - 100 * d - 208 * e + 128) >> 8).clamp(0, 255);
    let b = ((298 * c + 516 * d + 128) >> 8).clamp(0, 255);
    rgb.extend_from_slice(&[r as u8, g as u8, b as u8]);
}

fn checked_len(width: u32, height: u32, bytes_per_pixel: u32) -> Result<usize, ConvertError> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(u64::from(bytes_per_pixel)))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(ConvertError::Overflow { width, height })
}

fn require_len(format: FrameFormat, data: &[u8], expected: usize) -> Result<(), ConvertError> {
    if data.len() < expected {
        return Err(ConvertError::FrameTooShort {
            format: format.pixel_format,
            width: format.width,
            height: format.height,
            expected,
            actual: data.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb24_and_bgra_are_packed() {
        assert_eq!(
            to_rgb24(FrameFormat::rgb24(2, 1), &[1, 2, 3, 4, 5, 6]).unwrap(),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert_eq!(
            to_rgb24(
                FrameFormat::new(PixelFormat::Bgra, 2, 1),
                &[1, 2, 3, 255, 4, 5, 6, 0]
            )
            .unwrap(),
            vec![3, 2, 1, 6, 5, 4]
        );
    }

    #[test]
    fn yuyv_black_white_and_chroma_are_converted() {
        let format = FrameFormat::new(PixelFormat::Yuyv, 2, 2);
        // Y=16,U=128,V=128 is limited-range black; Y=235 is white.
        let data = [
            16, 128, 235, 128, // row 0: black, white
            16, 128, 235, 128, // row 1
        ];
        let rgb = to_rgb24(format, &data).unwrap();
        assert_eq!(rgb.len(), 2 * 2 * 3);
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert_eq!(&rgb[3..6], &[255, 255, 255]);

        // A red-ish chroma pair is not decoded as black.
        let red = to_rgb24(
            FrameFormat::new(PixelFormat::Yuyv, 2, 2),
            &[81, 90, 81, 240, 81, 90, 81, 240],
        )
        .unwrap();
        assert!(red[0] > 200 && red[1] < 80 && red[2] < 80, "{red:?}");
    }

    #[test]
    fn nv12_black_and_white_are_converted() {
        let format = FrameFormat::new(PixelFormat::Nv12, 2, 2);
        let data = [
            16, 235, // Y row 0
            16, 235, // Y row 1
            128, 128, // UV
        ];
        let rgb = to_rgb24(format, &data).unwrap();
        assert_eq!(rgb.len(), 2 * 2 * 3);
        assert_eq!(&rgb[0..3], &[0, 0, 0]);
        assert_eq!(&rgb[3..6], &[255, 255, 255]);
    }

    #[test]
    fn mjpeg_is_decoded_to_rgb24() {
        let source = crate::encode_jpeg_rgb(&[200, 30, 40, 10, 220, 90], 2, 1, 95).unwrap();
        let rgb = to_rgb24(FrameFormat::new(PixelFormat::Mjpeg, 2, 1), &source).unwrap();
        assert_eq!(rgb.len(), 2 * 1 * 3);
        assert!(rgb[0] > 170 && rgb[1] < 70 && rgb[2] < 90, "{rgb:?}");
        assert!(rgb[3] < 70 && rgb[4] > 180 && rgb[5] > 50, "{rgb:?}");
    }

    #[test]
    fn short_and_mismatched_payloads_are_errors() {
        let error = to_rgb24(FrameFormat::rgb24(2, 2), &[0; 4]).unwrap_err();
        assert!(matches!(error, ConvertError::FrameTooShort { .. }));
        let error = to_rgb24(FrameFormat::new(PixelFormat::Yuyv, 3, 2), &[0; 12]).unwrap_err();
        assert!(matches!(error, ConvertError::OddDimensions { .. }));
        let error = to_rgb24(
            FrameFormat::new(PixelFormat::Mjpeg, 4, 4),
            &crate::encode_jpeg_rgb(&[0; 12], 2, 2, 90).unwrap(),
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::DecodedDimensions { .. }));
        let error = to_rgb24(FrameFormat::new(PixelFormat::H264, 2, 2), &[0; 16]).unwrap_err();
        assert_eq!(error, ConvertError::UnsupportedFormat(PixelFormat::H264));
    }
}
