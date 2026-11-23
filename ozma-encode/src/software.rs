// SPDX-License-Identifier: AGPL-3.0-only
//! Real software H.264 encoding via the bundled OpenH264 library.
//!
//! [`H264SoftwareEncoder`] is the software fallback for the M1 media path. It
//! implements [`ozma_media::Encoder`], accepts the raw layouts a V4L2 capture
//! device can actually produce (`MJPEG`, `NV12`, `YUYV`, `RGB24`, `BGRA`),
//! converts them to tightly packed I420, and emits Annex-B H.264 access units.
//!
//! # Low latency and rate control
//!
//! The encoder is configured for interactive use: OpenH264's screen-content
//! real-time usage, a baseline profile (no B-frames), a single encoder thread,
//! frame skipping enabled so the requested bitrate can be met, and no internal
//! periodic-IDR timer. Keyframe cadence is owned by this type so
//! [`ozma_media::Encoder::request_keyframe`] and the configured GOP behave
//! predictably.
//!
//! # H.264 input
//!
//! When the capture device already produces H.264, this type switches to
//! **pass-through** mode. Annex-B access units are forwarded byte-for-byte and
//! keyframes are identified by scanning for an IDR NAL unit (type 5), because
//! driver flags are not always reliable. Pass-through cannot synthesize a
//! keyframe, so [`ozma_media::Encoder::request_keyframe`] returns a structured
//! error in that mode.
//!
//! # Deliberate limits
//!
//! * SDR only: HDR requests are rejected with [`ozma_protocol::codes::ENCODER_UNAVAILABLE`].
//! * I420 4:2:0 output; P010/HEVC/AV1 inputs are not converted.
//! * Color is limited-range BT.601 for SD and BT.709 for HD, signalled in the
//!   SPS VUI. Raw `NV12`/`YUYV` frames are assumed to match that matrix and are
//!   copied through rather than re-matrixed.
//! * Input payloads must be tightly packed (no stride padding). V4L2 sources in
//!   this workspace copy only `bytesused` bytes and expose no stride, so a
//!   padded buffer is rejected rather than silently misread.
//! * Odd `RGB24`/`BGRA`/`MJPEG` dimensions are cropped down to an even size,
//!   which is what OpenH264 requires. `NV12`/`YUYV` require even dimensions.
//! * No hardware encoder is claimed or used. See the crate docs for the
//!   hardware backend status.
//!
//! # Build requirement
//!
//! `openh264-sys2` compiles OpenH264's bundled sources. On a target where that
//! cannot build, this crate cannot construct a software encoder and reports a
//! structured `encoder.unavailable` error at runtime; it never pretends to
//! compress. A platform that needs a different codec backend should implement
//! [`ozma_media::Encoder`] behind an FFmpeg (or similar) sidecar, not a fake
//! encoder.

use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use ozma_media::{EncodedFrame, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{
    codes, ApiError, Codec, ColorFormat, EncodeCapability, EncoderBackend, MediaFormat,
};

/// Namespaced code for a frame that the encoder rejected at runtime.
///
/// The shared protocol guarantees `encoder.unavailable` and
/// `encoder.probe_failed`; this crate adds `encoder.encode_failed` for
/// per-frame payload failures (malformed JPEG, non-Annex-B H.264, and similar)
/// so callers can distinguish "this frame is bad" from "this encoder is gone".
pub const ENCODE_FAILED: &str = "encoder.encode_failed";

/// Largest width advertised by the software fallback.
pub const MAX_WIDTH: u32 = 1920;
/// Largest height advertised by the software fallback.
pub const MAX_HEIGHT: u32 = 1080;
/// Highest frame rate advertised by the software fallback.
pub const MAX_FPS: u32 = 60;

/// Default GOP when the requested frame rate is unknown.
const DEFAULT_GOP: u32 = 30;

/// Limited-range color matrix used when converting RGB-like input to I420.
///
/// H.264's conventional choice is BT.601 for SD and BT.709 for HD. Raw
/// NV12/YUYV frames are copied through unchanged, so this only selects the VUI
/// advertised in the SPS; it is still the caller's responsibility to capture a
/// stream whose matrix matches the mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorMatrix {
    Bt601,
    Bt709,
}

impl ColorMatrix {
    fn for_frame(width: usize, height: usize) -> Self {
        if height >= 720 || width >= 1280 {
            Self::Bt709
        } else {
            Self::Bt601
        }
    }

    fn vui(self) -> VuiConfig {
        match self {
            Self::Bt601 => VuiConfig::bt601(),
            Self::Bt709 => VuiConfig::bt709(),
        }
    }
}

/// A real software H.264 encoder.
///
/// Construct one with [`H264SoftwareEncoder::new`], then drive it through the
/// [`ozma_media::Encoder`] trait. The type is `Send` and can be moved to a
/// pipeline worker; encoding is synchronous CPU work with no internal delay.
pub struct H264SoftwareEncoder {
    id: String,
    input: FrameFormat,
    output: MediaFormat,
    coded_width: usize,
    coded_height: usize,
    keyframe_interval: u32,
    frames_since_keyframe: u64,
    force_keyframe: bool,
    last_pts: Option<u64>,
    last_sequence: Option<u64>,
    encoded: u64,
    mode: Option<Mode>,
}

enum Mode {
    Software(Encoder),
    Passthrough,
}

impl fmt::Debug for H264SoftwareEncoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("H264SoftwareEncoder")
            .field("id", &self.id)
            .field("input", &self.input)
            .field("output", &self.output)
            .field(
                "mode",
                &if self.is_passthrough() {
                    "passthrough"
                } else {
                    "software"
                },
            )
            .field("keyframe_interval", &self.keyframe_interval)
            .field("encoded", &self.encoded)
            .finish()
    }
}

impl H264SoftwareEncoder {
    /// Build a software H.264 encoder for `input` honouring `request`.
    ///
    /// The encoded dimensions are the input dimensions rounded down to an even
    /// size (except for pass-through, which keeps the source dimensions).
    /// `request` supplies the frame rate and bitrate; a zero frame rate or
    /// bitrate is replaced with a conservative default instead of failing.
    ///
    /// # Errors
    ///
    /// Returns a structured [`ApiError`] when the requested codec is not H.264,
    /// the request asks for HDR, the input layout is not one this backend can
    /// convert, the dimensions are unusable, or OpenH264 cannot initialise.
    pub fn new(input: FrameFormat, request: MediaFormat) -> Result<Self, ApiError> {
        if request.codec != Codec::H264 {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "The software encoder produces H.264 only; `{}` was requested.",
                    request.codec.as_str()
                ),
            )
            .with_details(request.codec.as_str().to_owned())
            .with_remediation(
                "Request H.264, or use a backend that supports the requested codec.",
            ));
        }

        if request.hdr || request.color.is_hdr() {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                "The software H.264 encoder supports SDR (8-bit 4:2:0) only.",
            )
            .with_details("hdr".to_owned())
            .with_remediation(
                "Request an SDR format, or use a hardware encoder for HDR capture.",
            ));
        }

        if input.width == 0 || input.height == 0 {
            return Err(ApiError::invalid(format!(
                "The encoder input format must have non-zero dimensions, got {}x{}.",
                input.width, input.height
            )));
        }

        let passthrough = input.pixel_format == PixelFormat::H264;
        if !passthrough
            && !matches!(
                input.pixel_format,
                PixelFormat::Mjpeg
                    | PixelFormat::Nv12
                    | PixelFormat::Yuyv
                    | PixelFormat::Rgb24
                    | PixelFormat::Bgra
            )
        {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "The software H.264 encoder cannot convert `{}` input.",
                    input.pixel_format
                ),
            )
            .with_details(input.pixel_format.as_str().to_owned())
            .with_remediation(
                "Capture MJPEG, NV12, YUYV, RGB24, or BGRA, or pass H.264 through directly.",
            ));
        }

        let (coded_width, coded_height) = if passthrough {
            (
                usize::try_from(input.width).map_err(|_| dimension_error(input))?,
                usize::try_from(input.height).map_err(|_| dimension_error(input))?,
            )
        } else {
            coded_dimensions(input)?
        };

        let fps = if request.fps == 0 { 30 } else { request.fps };
        let bitrate_kbps = if request.bitrate_kbps == 0 {
            2_000
        } else {
            request.bitrate_kbps
        };
        let keyframe_interval = if request.fps == 0 {
            DEFAULT_GOP
        } else {
            request.fps.clamp(1, 600)
        };

        let mut output = MediaFormat::new(
            Codec::H264,
            u32::try_from(coded_width).unwrap_or(u32::MAX),
            u32::try_from(coded_height).unwrap_or(u32::MAX),
            fps,
            bitrate_kbps,
        );
        output.color = ColorFormat::Sdr;

        let (id, mode) = if passthrough {
            ("h264-passthrough".to_owned(), Mode::Passthrough)
        } else {
            let matrix = ColorMatrix::for_frame(coded_width, coded_height);
            let config = EncoderConfig::new()
                .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1_000)))
                .max_frame_rate(FrameRate::from_hz(fps as f32))
                .rate_control_mode(RateControlMode::Bitrate)
                .usage_type(UsageType::ScreenContentRealTime)
                .profile(Profile::Baseline)
                .vui(matrix.vui())
                .sps_pps_strategy(SpsPpsStrategy::ConstantId)
                // Keyframe cadence is driven by this type; disabling the
                // internal timer keeps forced keyframes and GOP accounting in
                // one place.
                .intra_frame_period(IntraFramePeriod::from_num_frames(0))
                // Frame skipping is what lets OpenH264 honour the requested
                // bitrate in real-time mode instead of merely warning about it.
                .skip_frames(true)
                .adaptive_quantization(false)
                .background_detection(false)
                .num_threads(1);
            let encoder = Encoder::with_api_config(OpenH264API::from_source(), config).map_err(
                |error| {
                    ApiError::new(
                        codes::ENCODER_UNAVAILABLE,
                        format!("Failed to initialise the OpenH264 software encoder: {error}"),
                    )
                    .with_details("openh264".to_owned())
                    .with_remediation(
                        "Check that the bundled OpenH264 library is available for this target and that the frame dimensions are usable.",
                    )
                },
            )?;
            ("openh264-software-h264".to_owned(), Mode::Software(encoder))
        };

        Ok(Self {
            id,
            input,
            output,
            coded_width,
            coded_height,
            keyframe_interval,
            frames_since_keyframe: 0,
            force_keyframe: false,
            last_pts: None,
            last_sequence: None,
            encoded: 0,
            mode: Some(mode),
        })
    }

    /// Set the periodic keyframe interval in frames.
    ///
    /// `0` disables periodic keyframes; the first frame is still an IDR and
    /// [`ozma_media::Encoder::request_keyframe`] still works. This can be
    /// changed after construction because the cadence is tracked here rather
    /// than inside OpenH264.
    pub fn with_keyframe_interval(mut self, frames: u32) -> Self {
        self.keyframe_interval = frames;
        self
    }

    /// The capability this backend can truthfully advertise.
    pub fn capability() -> EncodeCapability {
        EncodeCapability {
            codec: Codec::H264,
            backend: EncoderBackend::Software,
            hardware: false,
            max_width: MAX_WIDTH,
            max_height: MAX_HEIGHT,
            max_fps: MAX_FPS,
            hdr: false,
            profiles: vec!["baseline".to_owned(), "constrained baseline".to_owned()],
        }
    }

    /// True when the encoder forwards H.264 instead of compressing raw frames.
    pub fn is_passthrough(&self) -> bool {
        matches!(self.mode, Some(Mode::Passthrough))
    }

    /// Number of frames encoded so far.
    pub fn encoded_count(&self) -> u64 {
        self.encoded
    }

    /// The exact number of frames between periodic keyframes, or `0` when the
    /// cadence is disabled.
    pub fn keyframe_interval(&self) -> u32 {
        self.keyframe_interval
    }

    fn closed(&self) -> MediaError {
        MediaError::encode(self.id.as_str(), "encoder is closed")
    }
}

impl Drop for H264SoftwareEncoder {
    fn drop(&mut self) {
        // OpenH264 has no delayed frames in this configuration, so stopping is
        // just releasing the encoder. Dropping `Mode::Software` runs the C
        // cleanup in `openh264`; taking the option keeps this explicit.
        self.mode = None;
    }
}

#[async_trait]
impl ozma_media::Encoder for H264SoftwareEncoder {
    fn id(&self) -> &str {
        &self.id
    }

    fn input_format(&self) -> FrameFormat {
        self.input
    }

    fn output_format(&self) -> MediaFormat {
        self.output.clone()
    }

    async fn encode(&mut self, frame: Frame) -> Result<EncodedFrame, MediaError> {
        if !frame.format().feeds(&self.input) {
            return Err(MediaError::FormatMismatch {
                expected: self.input.to_string(),
                actual: frame.format().to_string(),
            });
        }

        let pts = frame.pts_micros();
        let sequence = frame.sequence();
        if self.last_pts.is_some_and(|last| pts < last) {
            return Err(encode_failed(
                format!(
                    "Encoder `{}` received PTS {pts} after {last}; timestamps must not go backwards.",
                    self.id,
                    last = self.last_pts.unwrap_or_default()
                ),
                "non-monotonic PTS".to_owned(),
            ));
        }
        if self.last_sequence.is_some_and(|last| sequence < last) {
            return Err(encode_failed(
                format!(
                    "Encoder `{}` received sequence {sequence} after {last}; sequence numbers must not go backwards.",
                    self.id,
                    last = self.last_sequence.unwrap_or_default()
                ),
                "non-monotonic sequence".to_owned(),
            ));
        }

        if self.mode.is_none() {
            return Err(self.closed());
        }
        let mode = self.mode.as_mut().expect("mode checked above");
        let encoded = match mode {
            Mode::Passthrough => {
                let types = annex_b_nal_types(frame.data());
                if types.is_empty() {
                    return Err(encode_failed(
                        format!(
                            "H.264 pass-through input for `{}` is not an Annex-B access unit.",
                            self.input
                        ),
                        "h264".to_owned(),
                    ));
                }
                // The IDR scan is the source of truth: some capture drivers
                // fail to set their KEYFRAME buffer flag.
                let keyframe = types.contains(&5);
                EncodedFrame::new(
                    self.output.clone(),
                    pts,
                    sequence,
                    keyframe,
                    frame.into_data(),
                )?
            }
            Mode::Software(encoder) => {
                let i420 = convert_to_i420(frame.format(), frame.data()).map_err(|message| {
                    encode_failed(
                        format!(
                            "Encoder `{}` rejected a {} frame: {message}",
                            self.id, self.input
                        ),
                        self.input.pixel_format.as_str().to_owned(),
                    )
                })?;
                if i420.width != self.coded_width || i420.height != self.coded_height {
                    return Err(encode_failed(
                        format!(
                            "Encoder `{}` converted a frame to {}x{} but was configured for {}x{}.",
                            self.id, i420.width, i420.height, self.coded_width, self.coded_height
                        ),
                        self.input.to_string(),
                    ));
                }

                let yuv = YUVBuffer::from_vec(i420.data, i420.width, i420.height);
                let periodic = self.keyframe_interval > 0
                    && self.frames_since_keyframe.saturating_add(1)
                        >= u64::from(self.keyframe_interval);
                if self.force_keyframe || periodic {
                    encoder.force_intra_frame();
                    self.force_keyframe = false;
                }

                let (mut data, mut frame_type) = {
                    let bitstream = encoder.encode(&yuv).map_err(|error| {
                        encode_failed(
                            format!("OpenH264 failed to encode a frame: {error}"),
                            "openh264".to_owned(),
                        )
                    })?;
                    (bitstream.to_vec(), bitstream.frame_type())
                };

                if data.is_empty() || frame_type == FrameType::Skip {
                    // Rate control may decide to skip a frame. The trait
                    // contract is one access unit per input, so ask for an IDR
                    // and encode the same frame again rather than returning an
                    // empty payload, which `EncodedFrame` forbids anyway.
                    encoder.force_intra_frame();
                    (data, frame_type) = {
                        let bitstream = encoder.encode(&yuv).map_err(|error| {
                            encode_failed(
                                format!("OpenH264 failed to encode a forced keyframe: {error}"),
                                "openh264".to_owned(),
                            )
                        })?;
                        (bitstream.to_vec(), bitstream.frame_type())
                    };
                }

                if data.is_empty() {
                    return Err(encode_failed(
                        format!("Encoder `{}` produced an empty access unit.", self.id),
                        "openh264".to_owned(),
                    ));
                }
                if frame_type == FrameType::Invalid {
                    return Err(encode_failed(
                        format!(
                            "Encoder `{}` reported an invalid frame type for a non-empty access unit.",
                            self.id
                        ),
                        "openh264".to_owned(),
                    ));
                }

                let types = annex_b_nal_types(&data);
                if types.is_empty() {
                    return Err(encode_failed(
                        format!(
                            "Encoder `{}` produced output without an Annex-B start code.",
                            self.id
                        ),
                        "openh264".to_owned(),
                    ));
                }
                let keyframe = types.contains(&5);
                EncodedFrame::new(
                    self.output.clone(),
                    pts,
                    sequence,
                    keyframe,
                    Bytes::from(data),
                )?
            }
        };

        self.encoded = self.encoded.saturating_add(1);
        if encoded.is_keyframe() {
            self.frames_since_keyframe = 0;
        } else {
            self.frames_since_keyframe = self.frames_since_keyframe.saturating_add(1);
        }
        self.last_pts = Some(pts);
        self.last_sequence = Some(sequence);
        Ok(encoded)
    }

    async fn request_keyframe(&mut self) -> Result<(), MediaError> {
        if self.mode.is_none() {
            return Err(self.closed());
        }
        match self.mode.as_ref() {
            Some(Mode::Software(_)) => {
                self.force_keyframe = true;
                Ok(())
            }
            Some(Mode::Passthrough) => Err(encode_failed(
                "H.264 pass-through cannot synthesize a keyframe; only the capture device can produce one.",
                "h264".to_owned(),
            )),
            None => unreachable!("mode checked above"),
        }
    }

    async fn flush(&mut self) -> Result<Vec<EncodedFrame>, MediaError> {
        if self.mode.is_none() {
            return Err(self.closed());
        }
        // This encoder is synchronous and emits exactly one access unit per
        // input frame, so there is nothing buffered to drain.
        Ok(Vec::new())
    }
}

fn dimension_error(input: FrameFormat) -> ApiError {
    ApiError::invalid(format!(
        "The encoder input dimensions {}x{} cannot be represented on this platform.",
        input.width, input.height
    ))
}

fn encode_failed(message: impl Into<String>, details: impl Into<String>) -> MediaError {
    let message = message.into();
    MediaError::Api(
        ApiError::new(ENCODE_FAILED, message)
            .with_details(details.into())
            .with_remediation(
                "Check the capture format and frame payload; the encoder rejected this frame.",
            ),
    )
}

/// Even dimensions OpenH264 can encode.
fn coded_dimensions(input: FrameFormat) -> Result<(usize, usize), ApiError> {
    let width = usize::try_from(input.width).map_err(|_| dimension_error(input))?;
    let height = usize::try_from(input.height).map_err(|_| dimension_error(input))?;

    let (width, height) = match input.pixel_format {
        // Subsampled layouts address chroma using the full frame geometry, so
        // cropping an odd row or column would make the layout invalid.
        PixelFormat::Nv12 | PixelFormat::Yuyv => {
            if width % 2 != 0 || height % 2 != 0 {
                return Err(ApiError::new(
                    codes::ENCODER_UNAVAILABLE,
                    format!(
                        "The software H.264 encoder needs even dimensions for {} input, got {}x{}.",
                        input.pixel_format, input.width, input.height
                    ),
                )
                .with_details(input.to_string())
                .with_remediation(
                    "Negotiate an even capture mode such as 1280x720 or 1920x1080.",
                ));
            }
            (width, height)
        }
        _ => (width & !1, height & !1),
    };

    if width < 2 || height < 2 {
        return Err(ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!(
                "The software H.264 encoder cannot encode a {}x{} frame.",
                input.width, input.height
            ),
        )
        .with_details(input.to_string())
        .with_remediation("Capture at least 2x2 pixels."));
    }

    Ok((width, height))
}

/// I420 planes with tightly packed rows.
#[derive(Debug)]
struct I420Frame {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl I420Frame {
    fn new(width: usize, height: usize) -> Self {
        debug_assert!(width % 2 == 0 && height % 2 == 0);
        Self {
            data: vec![0u8; width * height * 3 / 2],
            width,
            height,
        }
    }

    fn y_index(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    fn u_index(&self, x: usize, y: usize) -> usize {
        let chroma_width = self.width / 2;
        self.width * self.height + (y / 2) * chroma_width + x / 2
    }

    fn v_index(&self, x: usize, y: usize) -> usize {
        let chroma_width = self.width / 2;
        let chroma_height = self.height / 2;
        self.width * self.height + chroma_width * chroma_height + (y / 2) * chroma_width + x / 2
    }

    fn set_luma(&mut self, x: usize, y: usize, value: u8) {
        let index = self.y_index(x, y);
        self.data[index] = value;
    }

    fn set_chroma(&mut self, x: usize, y: usize, u: u8, v: u8) {
        let u_index = self.u_index(x, y);
        let v_index = self.v_index(x, y);
        self.data[u_index] = u;
        self.data[v_index] = v;
    }
}

/// Convert one captured payload to I420 for OpenH264.
///
/// Kept private so the public contract stays the [`ozma_media::Encoder`] trait;
/// unit tests exercise the individual layouts directly.
fn convert_to_i420(format: FrameFormat, data: &[u8]) -> Result<I420Frame, String> {
    let width = usize::try_from(format.width).map_err(|_| "width is too large".to_owned())?;
    let height = usize::try_from(format.height).map_err(|_| "height is too large".to_owned())?;
    let matrix = ColorMatrix::for_frame(width, height);

    match format.pixel_format {
        PixelFormat::Rgb24 => {
            let (coded_width, coded_height) = even_crop(width, height);
            rgb24_to_i420(data, width, height, coded_width, coded_height, matrix)
        }
        PixelFormat::Bgra => {
            let (coded_width, coded_height) = even_crop(width, height);
            bgra_to_i420(data, width, height, coded_width, coded_height, matrix)
        }
        PixelFormat::Nv12 => nv12_to_i420(data, width, height),
        PixelFormat::Yuyv => yuyv_to_i420(data, width, height),
        PixelFormat::Mjpeg => {
            let (rgb, decoded_width) = decode_mjpeg(data, width, height)?;
            let (coded_width, coded_height) = even_crop(width, height);
            rgb24_to_i420(
                &rgb,
                decoded_width,
                height,
                coded_width,
                coded_height,
                matrix,
            )
        }
        other => Err(format!("unsupported pixel format `{other}`")),
    }
}

fn even_crop(width: usize, height: usize) -> (usize, usize) {
    (width & !1, height & !1)
}

fn checked_len(parts: &[usize]) -> Result<usize, String> {
    parts.iter().try_fold(1usize, |total, value| {
        total
            .checked_mul(*value)
            .ok_or_else(|| "frame size overflows usize".to_owned())
    })
}

fn require_len(data: &[u8], expected: usize, layout: &str) -> Result<(), String> {
    if data.len() < expected {
        return Err(format!(
            "{} payload is {} bytes but the format needs at least {expected}",
            layout,
            data.len()
        ));
    }
    Ok(())
}

fn rgb24_to_i420(
    data: &[u8],
    stride: usize,
    source_height: usize,
    width: usize,
    height: usize,
    matrix: ColorMatrix,
) -> Result<I420Frame, String> {
    let needed = checked_len(&[stride, source_height, 3])?;
    require_len(data, needed, "RGB24")?;
    let mut frame = I420Frame::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let index = (y * stride + x) * 3;
            let (luma, _, _) = yuv_from_rgb(data[index], data[index + 1], data[index + 2], matrix);
            frame.set_luma(x, y, luma);
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut u_sum = 0u32;
            let mut v_sum = 0u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let index = ((y + dy) * stride + (x + dx)) * 3;
                    let (_, u, v) =
                        yuv_from_rgb(data[index], data[index + 1], data[index + 2], matrix);
                    u_sum += u32::from(u);
                    v_sum += u32::from(v);
                }
            }
            frame.set_chroma(x, y, ((u_sum + 2) / 4) as u8, ((v_sum + 2) / 4) as u8);
        }
    }
    Ok(frame)
}

fn bgra_to_i420(
    data: &[u8],
    stride: usize,
    source_height: usize,
    width: usize,
    height: usize,
    matrix: ColorMatrix,
) -> Result<I420Frame, String> {
    let needed = checked_len(&[stride, source_height, 4])?;
    require_len(data, needed, "BGRA")?;
    let mut frame = I420Frame::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let index = (y * stride + x) * 4;
            let (luma, _, _) = yuv_from_rgb(data[index + 2], data[index + 1], data[index], matrix);
            frame.set_luma(x, y, luma);
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut u_sum = 0u32;
            let mut v_sum = 0u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let index = ((y + dy) * stride + (x + dx)) * 4;
                    let (_, u, v) =
                        yuv_from_rgb(data[index + 2], data[index + 1], data[index], matrix);
                    u_sum += u32::from(u);
                    v_sum += u32::from(v);
                }
            }
            frame.set_chroma(x, y, ((u_sum + 2) / 4) as u8, ((v_sum + 2) / 4) as u8);
        }
    }
    Ok(frame)
}

fn nv12_to_i420(data: &[u8], width: usize, height: usize) -> Result<I420Frame, String> {
    if width % 2 != 0 || height % 2 != 0 {
        return Err(format!(
            "NV12 requires even dimensions, got {width}x{height}"
        ));
    }
    let pixels = checked_len(&[width, height])?;
    require_len(data, pixels + pixels / 2, "NV12")?;
    let mut frame = I420Frame::new(width, height);

    frame.data[..pixels].copy_from_slice(&data[..pixels]);
    let chroma = pixels / 4;
    for index in 0..chroma {
        let uv = pixels + index * 2;
        let u_index = pixels + index;
        let v_index = pixels + chroma + index;
        frame.data[u_index] = data[uv];
        frame.data[v_index] = data[uv + 1];
    }
    Ok(frame)
}

fn yuyv_to_i420(data: &[u8], width: usize, height: usize) -> Result<I420Frame, String> {
    if width % 2 != 0 || height % 2 != 0 {
        return Err(format!(
            "YUYV requires even dimensions, got {width}x{height}"
        ));
    }
    let needed = checked_len(&[width, height, 2])?;
    require_len(data, needed, "YUYV")?;
    let mut frame = I420Frame::new(width, height);

    for y in 0..height {
        for x in 0..width {
            let index = (y * width + x) * 2;
            frame.set_luma(x, y, data[index]);
        }
    }
    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let top_u = (y * width + x) * 2 + 1;
            let bottom_u = ((y + 1) * width + x) * 2 + 1;
            let u = (u32::from(data[top_u]) + u32::from(data[bottom_u]) + 1) / 2;
            let v = (u32::from(data[top_u + 2]) + u32::from(data[bottom_u + 2]) + 1) / 2;
            frame.set_chroma(x, y, u as u8, v as u8);
        }
    }
    Ok(frame)
}

/// Limited-range RGB -> YUV for the selected BT.601 (SD) or BT.709 (HD)
/// matrix. The same matrix is signalled in the SPS VUI.
#[inline]
fn yuv_from_rgb(r: u8, g: u8, b: u8, matrix: ColorMatrix) -> (u8, u8, u8) {
    let r = i32::from(r);
    let g = i32::from(g);
    let b = i32::from(b);
    let (y, u, v) = match matrix {
        // BT.601 limited range (SD).
        ColorMatrix::Bt601 => (
            ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16,
            ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128,
            ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128,
        ),
        // BT.709 limited range (HD).
        ColorMatrix::Bt709 => (
            ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16,
            ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128,
            ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128,
        ),
    };
    (clamp_u8(y), clamp_u8(u), clamp_u8(v))
}

#[inline]
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Decode a complete MJPEG frame and return tightly packed RGB24.
///
/// The decoded geometry must match the negotiated capture format; a mismatch
/// is a capture bug, not something to silently rescale in the encoder. The
/// returned width is the decoded width so callers can use it as the stride.
fn decode_mjpeg(data: &[u8], width: usize, height: usize) -> Result<(Vec<u8>, usize), String> {
    use jpeg_decoder::{Decoder, PixelFormat as JpegPixelFormat};

    let mut decoder = Decoder::new(std::io::Cursor::new(data));
    let pixel_limit = checked_len(&[width, height, 4])?.max(1 << 20);
    decoder.set_max_decoding_buffer_size(pixel_limit);
    let pixels = decoder
        .decode()
        .map_err(|error| format!("malformed MJPEG frame: {error}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| "malformed MJPEG frame: no image information".to_owned())?;

    let decoded_width = usize::from(info.width);
    let decoded_height = usize::from(info.height);
    if decoded_width != width || decoded_height != height {
        return Err(format!(
            "decoded MJPEG image is {}x{} but capture negotiated {}x{}",
            decoded_width, decoded_height, width, height
        ));
    }

    match info.pixel_format {
        JpegPixelFormat::RGB24 => {
            require_len(&pixels, checked_len(&[width, height, 3])?, "decoded MJPEG")?;
            Ok((pixels, decoded_width))
        }
        JpegPixelFormat::L8 => {
            require_len(&pixels, checked_len(&[width, height])?, "decoded MJPEG")?;
            let mut rgb = Vec::with_capacity(checked_len(&[width, height, 3])?);
            for value in pixels {
                rgb.extend_from_slice(&[value, value, value]);
            }
            Ok((rgb, decoded_width))
        }
        other => Err(format!(
            "unsupported MJPEG pixel format {other:?}; only 8-bit RGB and grayscale are converted"
        )),
    }
}

/// NAL unit types found in an Annex-B buffer, in order.
///
/// This is a conservative start-code scan, like the capture path's IDR check:
/// it does not parse SPS/PPS payloads. Valid Annex-B streams escape emulation
/// patterns, so a `00 00 01` sequence is a start code.
pub fn annex_b_nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        let header = index + start_len;
        if header < data.len() {
            types.push(data[header] & 0x1f);
        }
        index = header;
    }
    types
}

/// Whether an Annex-B access unit contains an IDR picture (NAL type 5).
pub fn annex_b_is_keyframe(data: &[u8]) -> bool {
    annex_b_nal_types(data).contains(&5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i420_pixel(frame: &I420Frame, x: usize, y: usize) -> (u8, u8, u8) {
        (
            frame.data[frame.y_index(x, y)],
            frame.data[frame.u_index(x, y)],
            frame.data[frame.v_index(x, y)],
        )
    }

    #[test]
    fn rgb_white_is_limited_range() {
        let frame = rgb24_to_i420(
            &[255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            2,
            2,
            2,
            2,
            ColorMatrix::Bt601,
        )
        .unwrap();
        let (y, u, v) = i420_pixel(&frame, 0, 0);
        assert_eq!((y, u, v), (235, 128, 128));
    }

    #[test]
    fn rgb_black_is_limited_range() {
        let frame = rgb24_to_i420(
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            2,
            2,
            2,
            2,
            ColorMatrix::Bt601,
        )
        .unwrap();
        assert_eq!(i420_pixel(&frame, 1, 1), (16, 128, 128));
    }

    #[test]
    fn bgra_red_is_converted() {
        // B=0, G=0, R=255, A=255 for each pixel.
        let pixel = [0u8, 0, 255, 255];
        let frame = bgra_to_i420(
            &[pixel, pixel, pixel, pixel].concat(),
            2,
            2,
            2,
            2,
            ColorMatrix::Bt601,
        )
        .unwrap();
        let (y, u, v) = i420_pixel(&frame, 0, 0);
        assert!((80..=84).contains(&y), "Y was {y}");
        assert!((88..=92).contains(&u), "U was {u}");
        assert!(v >= 238, "V was {v}");
    }

    #[test]
    fn hd_uses_bt709_and_sd_uses_bt601() {
        assert_eq!(ColorMatrix::for_frame(1920, 1080), ColorMatrix::Bt709);
        assert_eq!(ColorMatrix::for_frame(1280, 720), ColorMatrix::Bt709);
        assert_eq!(ColorMatrix::for_frame(640, 480), ColorMatrix::Bt601);
        assert_eq!(ColorMatrix::for_frame(320, 240), ColorMatrix::Bt601);
    }

    #[test]
    fn bt709_red_is_converted_with_hd_coefficients() {
        let (y, u, v) = yuv_from_rgb(255, 0, 0, ColorMatrix::Bt709);
        assert_eq!((y, u, v), (63, 102, 240));
    }

    #[test]
    fn nv12_planes_are_deinterleaved() {
        // 2x2 NV12: Y = 10,20 / 30,40; UV = (100, 200).
        let data = [10, 20, 30, 40, 100, 200];
        let frame = nv12_to_i420(&data, 2, 2).unwrap();
        assert_eq!(&frame.data[..4], &[10, 20, 30, 40]);
        assert_eq!(frame.data[4], 100);
        assert_eq!(frame.data[5], 200);
    }

    #[test]
    fn yuyv_pairs_are_averaged_vertically() {
        // Each row: Y0 U Y1 V.
        let data = [10, 100, 20, 200, 30, 110, 40, 210];
        let frame = yuyv_to_i420(&data, 2, 2).unwrap();
        assert_eq!(&frame.data[..4], &[10, 20, 30, 40]);
        assert_eq!(frame.data[4], 105);
        assert_eq!(frame.data[5], 205);
    }

    #[test]
    fn conversion_rejects_truncated_payloads() {
        let format = FrameFormat::new(PixelFormat::Nv12, 16, 16);
        let error = convert_to_i420(format, &[0; 10]).unwrap_err();
        assert!(error.contains("at least"), "{error}");
    }

    #[test]
    fn odd_rgb_dimensions_are_cropped_to_even() {
        let (width, height) = even_crop(65, 33);
        assert_eq!((width, height), (64, 32));
    }

    #[test]
    fn nal_scan_finds_sps_pps_and_idr() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 1, 0x68, 3, // PPS
            0, 0, 1, 0x65, 4, // IDR
        ];
        assert_eq!(annex_b_nal_types(&data), vec![7, 8, 5]);
        assert!(annex_b_is_keyframe(&data));
        assert!(!annex_b_is_keyframe(&[0, 0, 1, 0x41, 0x00]));
    }

    #[test]
    fn nal_scan_ignores_data_without_start_codes() {
        assert!(annex_b_nal_types(b"not h264").is_empty());
    }
}
