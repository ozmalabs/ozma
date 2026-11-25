// SPDX-License-Identifier: AGPL-3.0-only
//! One-shot JPEG encoding for the controller's `/display/snapshot` route.
//!
//! The controller consumes a raw `image/jpeg` body, so the snapshot path must
//! produce real pixels rather than a placeholder. Only layouts this crate can
//! interpret without guessing are accepted:
//!
//! * `rgb24` is encoded directly;
//! * `bgra` is encoded directly (the alpha byte is ignored by JPEG);
//! * `mjpeg` is passed through when it already starts with the JPEG SOI marker;
//! * `yuyv` and `nv12` are converted to packed RGB24 first.
//!
//! Everything else returns [`JpegError::UnsupportedFormat`]. Callers must map
//! that to a typed error; silently treating unsupported compressed/P010 bytes
//! as RGB would produce a corrupt image.

use ozma_media::{Frame, PixelFormat};

/// JPEG SOI marker (`FF D8`) every encoded or passed-through image starts with.
const JPEG_SOI: [u8; 2] = [0xFF, 0xD8];

/// A snapshot JPEG could not be produced.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JpegError {
    /// A frame with no pixels.
    #[error("cannot encode a JPEG from an empty frame")]
    Empty,
    /// Width or height is zero.
    #[error("JPEG dimensions must be non-zero, got {width}x{height}")]
    ZeroDimensions {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
    /// JPEG dimensions are stored as `u16` in the encoder.
    #[error("JPEG dimensions exceed 65535 pixels: {width}x{height}")]
    DimensionsTooLarge {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
    },
    /// The frame layout is not a supported RGB/YUV layout or JPEG pass-through.
    #[error(
        "cannot encode `{0}` frames as JPEG; only rgb24, bgra, yuyv, nv12, and mjpeg are supported"
    )]
    UnsupportedFormat(PixelFormat),
    /// The buffer is shorter than the geometry requires.
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
    /// An `mjpeg` frame did not carry a JPEG stream.
    #[error("mjpeg frame does not start with a JPEG SOI marker")]
    NotJpeg,
    /// A YUV capture layout could not be converted to RGB24.
    #[error("cannot convert `{format}` frames to RGB24 for JPEG encoding: {message}")]
    Convert {
        /// Capture layout.
        format: PixelFormat,
        /// Conversion failure.
        message: String,
    },
    /// The encoder rejected the input.
    #[error("JPEG encoding failed: {0}")]
    Encode(String),
}

/// Encode one capture frame as a JPEG.
///
/// `quality` is clamped to `1..=100`.
pub fn encode_jpeg_frame(frame: &Frame, quality: u8) -> Result<Vec<u8>, JpegError> {
    let format = frame.format();
    match format.pixel_format {
        PixelFormat::Rgb24 => encode_jpeg_rgb(frame.data(), format.width, format.height, quality),
        PixelFormat::Bgra => encode_jpeg_bgra(frame.data(), format.width, format.height, quality),
        PixelFormat::Mjpeg => {
            if !frame.data().starts_with(&JPEG_SOI) {
                return Err(JpegError::NotJpeg);
            }
            Ok(frame.data().to_vec())
        }
        PixelFormat::Yuyv | PixelFormat::Nv12 => {
            let rgb =
                crate::to_rgb24(format, frame.data()).map_err(|error| JpegError::Convert {
                    format: format.pixel_format,
                    message: error.to_string(),
                })?;
            encode_jpeg_rgb(&rgb, format.width, format.height, quality)
        }
        other => Err(JpegError::UnsupportedFormat(other)),
    }
}

/// Encode packed RGB24 pixels as a JPEG.
pub fn encode_jpeg_rgb(
    data: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, JpegError> {
    encode(
        data,
        width,
        height,
        PixelFormat::Rgb24,
        3,
        jpeg_encoder::ColorType::Rgb,
        quality,
    )
}

/// Encode packed BGRA pixels as a JPEG. The alpha channel is ignored.
pub fn encode_jpeg_bgra(
    data: &[u8],
    width: u32,
    height: u32,
    quality: u8,
) -> Result<Vec<u8>, JpegError> {
    encode(
        data,
        width,
        height,
        PixelFormat::Bgra,
        4,
        jpeg_encoder::ColorType::Bgra,
        quality,
    )
}

fn encode(
    data: &[u8],
    width: u32,
    height: u32,
    format: PixelFormat,
    bytes_per_pixel: usize,
    color_type: jpeg_encoder::ColorType,
    quality: u8,
) -> Result<Vec<u8>, JpegError> {
    if width == 0 || height == 0 {
        return Err(JpegError::ZeroDimensions { width, height });
    }
    let width_u16 =
        u16::try_from(width).map_err(|_| JpegError::DimensionsTooLarge { width, height })?;
    let height_u16 =
        u16::try_from(height).map_err(|_| JpegError::DimensionsTooLarge { width, height })?;
    let expected = usize::try_from(u64::from(width) * u64::from(height))
        .ok()
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or(JpegError::DimensionsTooLarge { width, height })?;
    if data.is_empty() {
        return Err(JpegError::Empty);
    }
    if data.len() < expected {
        return Err(JpegError::FrameTooShort {
            format,
            width,
            height,
            expected,
            actual: data.len(),
        });
    }

    let mut output = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut output, quality.clamp(1, 100));
    encoder
        .encode(&data[..expected], width_u16, height_u16, color_type)
        .map_err(|error| JpegError::Encode(error.to_string()))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_media::{Frame, FrameFormat};
    use std::io::Cursor;

    fn decode_dimensions(jpeg: &[u8]) -> (u16, u16) {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
        let pixels = decoder.decode().expect("valid JPEG");
        let info = decoder.info().expect("JPEG info");
        assert!(!pixels.is_empty());
        (info.width, info.height)
    }

    #[test]
    fn encodes_rgb24_and_bgra_frames_with_real_dimensions() {
        let rgb =
            Frame::with_data(FrameFormat::rgb24(3, 2), 0, 0, true, vec![0u8; 3 * 2 * 3]).unwrap();
        let jpeg = encode_jpeg_frame(&rgb, 85).unwrap();
        assert!(jpeg.starts_with(&JPEG_SOI));
        assert_eq!(decode_dimensions(&jpeg), (3, 2));

        let bgra = Frame::with_data(
            FrameFormat::new(PixelFormat::Bgra, 2, 3),
            0,
            0,
            true,
            vec![0u8; 2 * 3 * 4],
        )
        .unwrap();
        let jpeg = encode_jpeg_frame(&bgra, 85).unwrap();
        assert!(jpeg.starts_with(&JPEG_SOI));
        assert_eq!(decode_dimensions(&jpeg), (2, 3));
    }

    #[test]
    fn passes_through_an_existing_mjpeg_frame() {
        let source = encode_jpeg_rgb(&[1, 2, 3, 4, 5, 6], 2, 1, 80).unwrap();
        let frame = Frame::with_data(
            FrameFormat::new(PixelFormat::Mjpeg, 2, 1),
            0,
            0,
            true,
            source.clone(),
        )
        .unwrap();
        assert_eq!(encode_jpeg_frame(&frame, 80).unwrap(), source);
    }

    #[test]
    fn rejects_non_jpeg_mjpeg_payloads() {
        let frame = Frame::with_data(
            FrameFormat::new(PixelFormat::Mjpeg, 2, 1),
            0,
            0,
            true,
            vec![0u8; 16],
        )
        .unwrap();
        assert_eq!(encode_jpeg_frame(&frame, 80), Err(JpegError::NotJpeg));
    }

    #[test]
    fn yuyv_and_nv12_frames_are_converted_and_encoded() {
        let yuyv = Frame::with_data(
            FrameFormat::new(PixelFormat::Yuyv, 2, 2),
            0,
            0,
            true,
            vec![
                16, 128, 235, 128, // row 0: limited-range black, then white
                16, 128, 235, 128, // row 1
            ],
        )
        .unwrap();
        let jpeg = encode_jpeg_frame(&yuyv, 90).unwrap();
        assert!(jpeg.starts_with(&JPEG_SOI));
        assert_eq!(decode_dimensions(&jpeg), (2, 2));
        let pixels = jpeg_decoder::Decoder::new(Cursor::new(&jpeg))
            .decode()
            .expect("valid YUYV JPEG");
        assert!(
            pixels[0] < 40 && pixels[1] < 40 && pixels[2] < 40,
            "{pixels:?}"
        );
        assert!(
            pixels[3] > 210 && pixels[4] > 210 && pixels[5] > 210,
            "{pixels:?}"
        );

        let nv12 = Frame::with_data(
            FrameFormat::new(PixelFormat::Nv12, 2, 2),
            0,
            0,
            true,
            vec![
                16, 235, // Y row 0: black, white
                16, 235, // Y row 1
                128, 128, // neutral chroma
            ],
        )
        .unwrap();
        let jpeg = encode_jpeg_frame(&nv12, 90).unwrap();
        assert!(jpeg.starts_with(&JPEG_SOI));
        assert_eq!(decode_dimensions(&jpeg), (2, 2));
        let pixels = jpeg_decoder::Decoder::new(Cursor::new(&jpeg))
            .decode()
            .expect("valid NV12 JPEG");
        assert!(
            pixels[0] < 40 && pixels[1] < 40 && pixels[2] < 40,
            "{pixels:?}"
        );
        assert!(
            pixels[3] > 210 && pixels[4] > 210 && pixels[5] > 210,
            "{pixels:?}"
        );
    }

    #[test]
    fn short_yuv_payloads_are_typed_conversion_errors() {
        let frame = Frame::with_data(
            FrameFormat::new(PixelFormat::Nv12, 4, 4),
            0,
            0,
            true,
            vec![0u8; 4],
        )
        .unwrap();
        assert!(matches!(
            encode_jpeg_frame(&frame, 80),
            Err(JpegError::Convert {
                format: PixelFormat::Nv12,
                ..
            })
        ));
    }

    #[test]
    fn rejects_unsupported_and_short_frames_with_typed_errors() {
        let h264 = Frame::with_data(
            FrameFormat::new(PixelFormat::H264, 2, 2),
            0,
            0,
            true,
            vec![0u8; 16],
        )
        .unwrap();
        assert_eq!(
            encode_jpeg_frame(&h264, 80),
            Err(JpegError::UnsupportedFormat(PixelFormat::H264))
        );

        assert!(matches!(
            encode_jpeg_rgb(&[0u8; 2], 2, 1, 80),
            Err(JpegError::FrameTooShort {
                format: PixelFormat::Rgb24,
                expected: 6,
                actual: 2,
                ..
            })
        ));
        assert!(matches!(
            encode_jpeg_rgb(&[], 0, 1, 80),
            Err(JpegError::ZeroDimensions { .. })
        ));
    }
}
