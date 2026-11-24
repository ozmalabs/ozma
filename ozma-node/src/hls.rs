// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Local HLS pipeline: a [`CaptureSource`] → ffmpeg `rawvideo` → HLS.
//!
//! This is the controller-compatibility stream path. The Python controller
//! proxies `GET /stream/{file}` from a node's API port without credentials
//! (`controller/api.py::stream_hls_proxy`), so the node serves the HLS output
//! directory verbatim and unauthenticated. That is media, not control: input,
//! power, and the native API stay behind the bearer guard and the controller's
//! own authorization.
//!
//! The pipeline is deliberately small and honest:
//!
//! * ffmpeg reads packed RGB24 frames from stdin and writes a rolling HLS
//!   playlist (`stream.m3u8`) plus `seg_%05d.ts` segments.
//! * [`HlsConfig::validate`] rejects values ffmpeg would misinterpret.
//! * Only `rgb24` and `bgra` sources are accepted. `bgra` is converted to
//!   `rgb24`; MJPEG/YUYV/NV12 are rejected with a typed error rather than being
//!   mislabelled as RGB and fed to the encoder.
//! * A source whose frame size changes (a guest reset or display mode switch)
//!   restarts ffmpeg with the new `-s WxH` instead of failing. The new
//!   generation appends to the existing playlist with
//!   `-hls_flags delete_segments+append_list+discont_start`, so old segments
//!   stay playable and the switch is marked as an HLS discontinuity. This is
//!   only automatic when [`HlsConfig::width`]/[`HlsConfig::height`] are unset;
//!   explicitly pinned dimensions are a contract, and a mismatched frame is a
//!   typed [`HlsError::FrameDimensions`] failure (no silent scaling).
//! * [`HlsStatus`] reports `running` only after the manifest exists. A spawned
//!   ffmpeg whose playlist has not been written yet is `failed`, not a success.
//!
//! This is not WebRTC and it is not the native session plane. It is the
//! temporary compatibility surface described in
//! `docs/rust-reconciliation-plan.md` §3 Phase 2.

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tower_http::services::ServeDir;

/// Default HLS segment duration in seconds.
pub const DEFAULT_SEGMENT_SECONDS: f32 = 1.0;
/// Default number of playlist entries.
pub const DEFAULT_PLAYLIST_SIZE: u32 = 4;
/// Default capture/encode frame rate.
pub const DEFAULT_FPS: u32 = 30;
/// Default video bitrate in kbps (4 Mbps).
pub const DEFAULT_BITRATE_KBPS: u32 = 4_000;
/// Largest error string retained for `/api/v1/connection`.
const MAX_STATUS_ERROR_CHARS: usize = 512;

/// Largest GOP value ffmpeg accepts for `-g`/`-keyint_min`.
///
/// Both are signed 32-bit `AVCodecContext` options, so a larger value would
/// make ffmpeg reject the whole argv.
const MAX_GOP_FRAMES: u32 = i32::MAX as u32;

/// Video encoder used by the HLS ffmpeg pipeline.
///
/// The software variants are the historical path. The hardware variants name
/// the exact ffmpeg encoder and carry the honest upload/device options the raw
/// RGB24 stdin path needs. They are never assumed to work: a hardware request
/// is only started after an injected [`EncoderProbe`] confirms that ffmpeg
/// advertises the encoder (see [`HlsController`] and [`HlsPipeline`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HlsEncoder {
    /// Software H.264 through `libx264` (the historical default).
    #[default]
    H264,
    /// Software HEVC through `libx265`.
    Hevc,
    /// Software AV1 through `libsvtav1`.
    Av1Svt,
    /// Software AV1 through `libaom-av1`.
    Av1Aom,
    /// Motion JPEG through `mjpeg`.
    Mjpeg,
    /// VAAPI H.264 through `h264_vaapi` (uploads nv12 frames).
    VaapiH264,
    /// VAAPI HEVC through `hevc_vaapi` (uploads nv12 frames).
    VaapiHevc,
    /// NVENC H.264 through `h264_nvenc`.
    NvencH264,
    /// NVENC HEVC through `hevc_nvenc`.
    NvencHevc,
    /// Intel QSV H.264 through `h264_qsv`.
    QsvH264,
    /// Intel QSV HEVC through `hevc_qsv`.
    QsvHevc,
    /// V4L2 memory-to-memory H.264 through `h264_v4l2m2m`.
    V4l2m2mH264,
    /// V4L2 memory-to-memory HEVC through `hevc_v4l2m2m`.
    V4l2m2mHevc,
}

/// Video codec family a hardware `hw_accel` alias can be paired with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HwCodec {
    H264,
    Hevc,
}

impl HlsEncoder {
    /// Every encoder this pipeline supports, in preference order.
    pub const ALL: [Self; 13] = [
        Self::H264,
        Self::Hevc,
        Self::Av1Svt,
        Self::Av1Aom,
        Self::Mjpeg,
        Self::VaapiH264,
        Self::VaapiHevc,
        Self::NvencH264,
        Self::NvencHevc,
        Self::QsvH264,
        Self::QsvHevc,
        Self::V4l2m2mH264,
        Self::V4l2m2mHevc,
    ];

    /// The exact `ffmpeg -c:v` value.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::H264 => "libx264",
            Self::Hevc => "libx265",
            Self::Av1Svt => "libsvtav1",
            Self::Av1Aom => "libaom-av1",
            Self::Mjpeg => "mjpeg",
            Self::VaapiH264 => "h264_vaapi",
            Self::VaapiHevc => "hevc_vaapi",
            Self::NvencH264 => "h264_nvenc",
            Self::NvencHevc => "hevc_nvenc",
            Self::QsvH264 => "h264_qsv",
            Self::QsvHevc => "hevc_qsv",
            Self::V4l2m2mH264 => "h264_v4l2m2m",
            Self::V4l2m2mHevc => "hevc_v4l2m2m",
        }
    }

    /// True for every backend that needs a GPU/V4L2 encoder device.
    pub fn is_hardware(self) -> bool {
        !matches!(
            self,
            Self::H264 | Self::Hevc | Self::Av1Svt | Self::Av1Aom | Self::Mjpeg
        )
    }

    /// True when this encoder requires a VAAPI render node.
    pub fn is_vaapi(self) -> bool {
        matches!(self, Self::VaapiH264 | Self::VaapiHevc)
    }

    /// Stable hardware-backend label (`vaapi`, `nvenc`, `qsv`, `v4l2m2m`).
    pub fn backend_label(self) -> &'static str {
        match self {
            Self::VaapiH264 | Self::VaapiHevc => "vaapi",
            Self::NvencH264 | Self::NvencHevc => "nvenc",
            Self::QsvH264 | Self::QsvHevc => "qsv",
            Self::V4l2m2mH264 | Self::V4l2m2mHevc => "v4l2m2m",
            _ => "software",
        }
    }

    /// The software encoder with the same codec family, when this is hardware.
    ///
    /// Only H.264/HEVC have a matching software encoder in this pipeline. A
    /// caller must opt in explicitly (see
    /// [`HlsController::restart_with_encoder_fallback`]); this method never
    /// changes a selection on its own.
    pub fn software_fallback(self) -> Option<Self> {
        match self {
            Self::VaapiH264 | Self::NvencH264 | Self::QsvH264 | Self::V4l2m2mH264 => {
                Some(Self::H264)
            }
            Self::VaapiHevc | Self::NvencHevc | Self::QsvHevc | Self::V4l2m2mHevc => {
                Some(Self::Hevc)
            }
            _ => None,
        }
    }

    /// Pixel format the HLS muxer should receive for this encoder.
    ///
    /// The capture source is RGB24. Without an explicit target, ffmpeg may
    /// negotiate a 4:4:4 format (`libx264` → `yuv444p`, NVENC → `gbrp`) that
    /// most HLS/browser decoders cannot play. VAAPI already uploads NV12 via
    /// its filter; QSV wants NV12; every other video encoder gets 4:2:0.
    /// MJPEG keeps ffmpeg's default (`yuvj420p`/`yuvj444p`).
    pub fn hls_pixel_format(self) -> Option<&'static str> {
        match self {
            Self::H264 | Self::Hevc | Self::Av1Svt | Self::Av1Aom => Some("yuv420p"),
            Self::VaapiH264 | Self::VaapiHevc => None,
            Self::NvencH264 | Self::NvencHevc | Self::V4l2m2mH264 | Self::V4l2m2mHevc => {
                Some("yuv420p")
            }
            Self::QsvH264 | Self::QsvHevc => Some("nv12"),
            Self::Mjpeg => None,
        }
    }

    /// Parse a controller/CLI encoder name.
    ///
    /// This accepts software names, the explicit hardware ffmpeg names
    /// (`h264_vaapi`, `hevc_nvenc`, `h264_qsv`, `hevc_v4l2m2m`, ...), and a
    /// bare `hw_accel` alias only when [`Self::parse_with_codec`] supplies the
    /// codec. A bare alias is a typed error rather than a guess.
    pub fn parse(value: &str) -> Result<Self, HlsError> {
        Self::parse_with_codec(value, None)
    }

    /// Resolve a controller `hw_accel` alias plus its codec in one call.
    ///
    /// Equivalent to [`Self::parse_with_codec`] with a required codec.
    pub fn parse_hw_accel(alias: &str, codec: &str) -> Result<Self, HlsError> {
        Self::parse_with_codec(alias, Some(codec))
    }

    /// Parse an encoder name with an optional codec hint.
    ///
    /// `hw_accel` aliases (`vaapi`, `nvenc`, `qsv`, `v4l2m2m`) are resolved
    /// against `codec` (`h264`/`hevc`, including the usual aliases). An invalid
    /// hardware/codec combination, a bare alias without a codec, and unknown
    /// values all return [`HlsError::UnsupportedEncoder`].
    pub fn parse_with_codec(value: &str, codec: Option<&str>) -> Result<Self, HlsError> {
        let normalized = value.trim().to_ascii_lowercase();
        if let Some(alias) = hw_alias(&normalized) {
            let codec = codec
                .map(str::trim)
                .filter(|codec| !codec.is_empty())
                .ok_or_else(|| HlsError::UnsupportedEncoder {
                    encoder: value.trim().to_owned(),
                    message: format!(
                        "hw_accel `{normalized}` needs a codec; pass h264 or hevc (for example \
                         `--hls-encoder h264_{}` or a `/codec` request with `codec` set)",
                        alias.ffmpeg_suffix()
                    ),
                })?;
            return Self::from_hw_accel(alias, codec, value);
        }

        match normalized.as_str() {
            "libx264" | "x264" | "h264" | "avc" | "h.264" => Ok(Self::H264),
            "libx265" | "x265" | "h265" | "hevc" | "h.265" => Ok(Self::Hevc),
            "libsvtav1" | "svtav1" | "av1" => Ok(Self::Av1Svt),
            "libaom-av1" | "aom-av1" | "libaom" => Ok(Self::Av1Aom),
            "mjpeg" | "mjpg" | "jpeg" => Ok(Self::Mjpeg),
            "h264_vaapi" | "vaapi_h264" => Ok(Self::VaapiH264),
            "hevc_vaapi" | "h265_vaapi" | "vaapi_hevc" => Ok(Self::VaapiHevc),
            "h264_nvenc" | "nvenc_h264" => Ok(Self::NvencH264),
            "hevc_nvenc" | "h265_nvenc" | "nvenc_hevc" => Ok(Self::NvencHevc),
            "h264_qsv" | "qsv_h264" => Ok(Self::QsvH264),
            "hevc_qsv" | "h265_qsv" | "qsv_hevc" => Ok(Self::QsvHevc),
            "h264_v4l2m2m" | "v4l2m2m_h264" => Ok(Self::V4l2m2mH264),
            "hevc_v4l2m2m" | "h265_v4l2m2m" | "v4l2m2m_hevc" => Ok(Self::V4l2m2mHevc),
            _ => Err(HlsError::UnsupportedEncoder {
                encoder: value.trim().to_owned(),
                message: format!(
                    "expected one of {}, or a hw_accel alias (vaapi, nvenc, qsv, v4l2m2m) with \
                     h264/hevc",
                    Self::ALL
                        .iter()
                        .map(|encoder| encoder.ffmpeg_name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }
    }

    /// Resolve a `hw_accel` alias plus a codec name.
    pub fn from_hw_accel(alias: HwAccel, codec: &str, supplied: &str) -> Result<Self, HlsError> {
        let codec = match codec.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" | "h.264" => HwCodec::H264,
            "h265" | "hevc" | "h.265" => HwCodec::Hevc,
            other => {
                return Err(HlsError::UnsupportedEncoder {
                    encoder: supplied.trim().to_owned(),
                    message: format!(
                        "hw_accel `{alias}` only supports h264 or hevc in the HLS pipeline, not \
                         `{other}`"
                    ),
                })
            }
        };
        Ok(match (alias, codec) {
            (HwAccel::Vaapi, HwCodec::H264) => Self::VaapiH264,
            (HwAccel::Vaapi, HwCodec::Hevc) => Self::VaapiHevc,
            (HwAccel::Nvenc, HwCodec::H264) => Self::NvencH264,
            (HwAccel::Nvenc, HwCodec::Hevc) => Self::NvencHevc,
            (HwAccel::Qsv, HwCodec::H264) => Self::QsvH264,
            (HwAccel::Qsv, HwCodec::Hevc) => Self::QsvHevc,
            (HwAccel::V4l2m2m, HwCodec::H264) => Self::V4l2m2mH264,
            (HwAccel::V4l2m2m, HwCodec::Hevc) => Self::V4l2m2mHevc,
        })
    }
}

/// A controller/CLI hardware acceleration alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwAccel {
    /// VAAPI (`vaapi`).
    Vaapi,
    /// NVIDIA NVENC (`nvenc`).
    Nvenc,
    /// Intel Quick Sync (`qsv`).
    Qsv,
    /// V4L2 memory-to-memory (`v4l2m2m`).
    V4l2m2m,
}

impl HwAccel {
    fn ffmpeg_suffix(self) -> &'static str {
        match self {
            Self::Vaapi => "vaapi",
            Self::Nvenc => "nvenc",
            Self::Qsv => "qsv",
            Self::V4l2m2m => "v4l2m2m",
        }
    }
}

impl std::fmt::Display for HwAccel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.ffmpeg_suffix())
    }
}

fn hw_alias(value: &str) -> Option<HwAccel> {
    match value {
        "vaapi" => Some(HwAccel::Vaapi),
        "nvenc" => Some(HwAccel::Nvenc),
        "qsv" => Some(HwAccel::Qsv),
        "v4l2m2m" | "v4l2-m2m" => Some(HwAccel::V4l2m2m),
        _ => None,
    }
}

/// Configuration for the ffmpeg-backed HLS pipeline.
///
/// `width`/`height` optionally pin the input size advertised to ffmpeg. When
/// they are unset the source's declared format is used, or — for sources such
/// as the QMP display, which reports `0x0` until the first screendump — the
/// size of the first frame is learned lazily. In that dynamic mode a later
/// frame of another size restarts ffmpeg at the new size ([`HlsPipeline`]).
/// When both are set they are authoritative and a different frame size is a
/// typed [`HlsError::FrameDimensions`] failure; the pipeline does not scale.
#[derive(Clone, Debug, PartialEq)]
pub struct HlsConfig {
    /// Directory receiving `stream.m3u8` and `seg_%05d.ts`.
    pub output_dir: PathBuf,
    /// Target segment duration in seconds.
    pub segment_seconds: f32,
    /// Number of segments retained in the playlist.
    pub playlist_size: u32,
    /// Input frame rate advertised to ffmpeg.
    pub fps: u32,
    /// Video bitrate in kbps (ignored by the constant-quality MJPEG path).
    pub bitrate_kbps: u32,
    /// Video encoder selected for this pipeline generation.
    pub video_encoder: HlsEncoder,
    /// Optional input width override.
    pub width: Option<u32>,
    /// Optional input height override.
    pub height: Option<u32>,
    /// ffmpeg executable to spawn (a bare `ffmpeg` uses `PATH`).
    pub ffmpeg_bin: PathBuf,
    /// VAAPI render node used by the VAAPI encoder variants.
    ///
    /// Only checked for existence when a pipeline starts/restarts, never while
    /// building argv, so tests can construct argv without a real GPU node.
    pub vaapi_device: PathBuf,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            output_dir: std::env::temp_dir().join("ozma-stream"),
            segment_seconds: DEFAULT_SEGMENT_SECONDS,
            playlist_size: DEFAULT_PLAYLIST_SIZE,
            fps: DEFAULT_FPS,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            video_encoder: HlsEncoder::default(),
            width: None,
            height: None,
            ffmpeg_bin: PathBuf::from("ffmpeg"),
            vaapi_device: PathBuf::from("/dev/dri/renderD128"),
        }
    }
}

impl HlsConfig {
    /// Reject values that ffmpeg cannot use or that make no sense.
    pub fn validate(&self) -> Result<(), HlsError> {
        if self.output_dir.as_os_str().is_empty()
            || self.output_dir.to_string_lossy().trim().is_empty()
        {
            return Err(HlsError::InvalidConfig(
                "--hls-dir must not be empty".to_owned(),
            ));
        }
        if self.ffmpeg_bin.as_os_str().is_empty()
            || self.ffmpeg_bin.to_string_lossy().trim().is_empty()
        {
            return Err(HlsError::InvalidConfig(
                "--ffmpeg-bin must not be empty".to_owned(),
            ));
        }
        if !self.segment_seconds.is_finite() || self.segment_seconds <= 0.0 {
            return Err(HlsError::InvalidConfig(
                "--hls-segment-seconds must be a finite value greater than zero".to_owned(),
            ));
        }
        if self.playlist_size == 0 {
            return Err(HlsError::InvalidConfig(
                "--hls-playlist-size must be at least 1".to_owned(),
            ));
        }
        if self.fps == 0 {
            return Err(HlsError::InvalidConfig(
                "--hls-fps must be at least 1".to_owned(),
            ));
        }
        if self.bitrate_kbps == 0 {
            return Err(HlsError::InvalidConfig(
                "--hls-bitrate-kbps must be at least 1".to_owned(),
            ));
        }
        match (self.width, self.height) {
            (Some(0), _) | (_, Some(0)) => {
                return Err(HlsError::InvalidConfig(
                    "HLS width and height must be greater than zero".to_owned(),
                ));
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(HlsError::InvalidConfig(
                    "HLS width and height must be configured together".to_owned(),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    fn validated(self) -> Result<Self, HlsError> {
        self.validate()?;
        Ok(self)
    }

    /// Keyframe interval in frames that keeps GOPs aligned with segments.
    ///
    /// `-hls_time` is only a target: libavformat can split a segment only at a
    /// keyframe, so the encoder's default GOP decides the real segment length
    /// (libx264 defaults to 250 frames, which is 50 seconds at 5 fps). This is
    /// `round(fps × segment_seconds)`, clamped to at least one frame and to
    /// [`MAX_GOP_FRAMES`] so extreme but valid configurations cannot overflow
    /// or produce an argv ffmpeg rejects.
    pub fn gop_frames(&self) -> u32 {
        let frames = f64::from(self.fps) * f64::from(self.segment_seconds);
        if !frames.is_finite() {
            return MAX_GOP_FRAMES;
        }
        let rounded = frames.round();
        if rounded < 1.0 {
            1
        } else if rounded > f64::from(MAX_GOP_FRAMES) {
            MAX_GOP_FRAMES
        } else {
            rounded as u32
        }
    }

    /// `-force_key_frames` expression that closes a segment on time even when
    /// `fps × segment_seconds` is fractional.
    pub fn force_key_frames_expr(&self) -> String {
        format!(
            "expr:gte(t,n_forced*{})",
            format_seconds(self.segment_seconds)
        )
    }

    /// Path of the HLS playlist that the controller proxies.
    pub fn manifest_path(&self) -> PathBuf {
        self.output_dir.join("stream.m3u8")
    }

    /// Path pattern of the HLS segments.
    pub fn segment_pattern(&self) -> PathBuf {
        self.output_dir.join("seg_%05d.ts")
    }
}

/// Errors from configuration validation, frame conversion, or ffmpeg I/O.
#[derive(Debug, thiserror::Error)]
pub enum HlsError {
    /// The configuration cannot be used.
    #[error("invalid HLS configuration: {0}")]
    InvalidConfig(String),
    /// The selected ffmpeg encoder is not supported by this pipeline.
    #[error("HLS encoder `{encoder}` is not supported: {message}")]
    UnsupportedEncoder {
        /// Encoder name as supplied.
        encoder: String,
        /// Why it cannot be used.
        message: String,
    },
    /// A hardware encoder was requested but ffmpeg does not advertise it.
    #[error("HLS hardware encoder `{encoder}` ({backend}) is unavailable: {message}")]
    EncoderUnavailable {
        /// Exact ffmpeg encoder name.
        encoder: String,
        /// Hardware backend label (`vaapi`, `nvenc`, `qsv`, `v4l2m2m`).
        backend: String,
        /// What the probe found.
        message: String,
        /// What the operator can do about it.
        remediation: String,
        /// Encoder names ffmpeg actually advertised, when known.
        available: Vec<String>,
    },
    /// A VAAPI encoder was requested but its render node does not exist.
    #[error("VAAPI device `{path}` is unavailable: {message}")]
    VaapiDevice {
        /// Missing or unreadable device path.
        path: PathBuf,
        /// What the check found.
        message: String,
        /// What the operator can do about it.
        remediation: String,
    },
    /// A fresh capture source could not be opened for a pipeline generation.
    #[error("could not open an HLS capture source: {message}")]
    Source {
        /// Underlying failure.
        message: String,
    },
    /// There is no running pipeline to restart.
    #[error("the HLS pipeline is not running")]
    NotRunning,
    /// A restart failed and the previous pipeline was not restored.
    #[error("HLS restart failed: {message}")]
    Restart {
        /// What failed, including the restoration outcome.
        message: String,
    },
    /// A pixel format cannot be fed to ffmpeg's raw RGB24 input.
    #[error(
        "HLS cannot feed `{0}` frames to ffmpeg as raw RGB24; only rgb24 and bgra are supported"
    )]
    UnsupportedFormat(PixelFormat),
    /// A frame's pixel layout differs from the source's declared format.
    #[error("frame format `{actual}` does not match the source's declared `{expected}` format")]
    FormatMismatch {
        /// Format the source declared.
        expected: PixelFormat,
        /// Format the frame actually carried.
        actual: PixelFormat,
    },
    /// A frame's byte length does not match its geometry.
    #[error(
        "frame buffer is {actual} bytes, expected {expected} for {format} at {width}x{height}"
    )]
    FrameSize {
        /// Frame layout.
        format: PixelFormat,
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Required byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
    /// A frame's dimensions differ from the size advertised to ffmpeg.
    ///
    /// With dynamic dimensions the pipeline restarts ffmpeg instead. This
    /// error is returned when [`HlsConfig::width`]/[`HlsConfig::height`] pinned
    /// the input size: the frame is rejected rather than silently scaled.
    #[error(
        "frame dimensions {width}x{height} do not match the HLS input {expected_width}x{expected_height}"
    )]
    FrameDimensions {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Size advertised to ffmpeg.
        expected_width: u32,
        /// Size advertised to ffmpeg.
        expected_height: u32,
    },
    /// The output directory could not be created.
    #[error("could not create HLS output directory `{path}`: {message}")]
    OutputDirectory {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying failure.
        message: String,
    },
    /// ffmpeg could not be spawned.
    #[error("could not start ffmpeg `{bin}`: {message}")]
    Spawn {
        /// Executable that failed to start.
        bin: PathBuf,
        /// Underlying failure.
        message: String,
    },
    /// The child process did not expose stdin.
    #[error("ffmpeg did not expose a writable stdin")]
    StdinUnavailable,
    /// The frame task panicked or was aborted unexpectedly.
    #[error("the HLS frame task failed: {message}")]
    Task {
        /// Underlying failure.
        message: String,
    },
}

/// Status retained for `/api/v1/connection`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HlsState {
    /// `--hls` was not requested.
    #[default]
    NotConfigured,
    /// The pipeline is starting or waiting for its first frame.
    Starting,
    /// Frames are reaching ffmpeg.
    Running,
    /// The pipeline could not start or stopped on an error.
    Failed,
    /// The pipeline stopped, including a clean source end-of-stream.
    Stopped,
}

#[derive(Debug, Default)]
struct HlsStatusInner {
    state: HlsState,
    error: Option<String>,
    manifest: Option<PathBuf>,
}

/// Cloneable, bounded status of the HLS pipeline.
///
/// The connection endpoint reads this without touching the frame task.
#[derive(Clone, Debug, Default)]
pub struct HlsStatus {
    inner: Arc<Mutex<HlsStatusInner>>,
}

impl HlsStatus {
    /// No pipeline was requested.
    pub fn not_configured() -> Self {
        Self::default()
    }

    /// A pipeline could not be started.
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HlsStatusInner {
                state: HlsState::Failed,
                error: Some(bounded(message.into())),
                manifest: None,
            })),
        }
    }

    fn starting(manifest: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HlsStatusInner {
                state: HlsState::Starting,
                error: None,
                manifest: Some(manifest),
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HlsStatusInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_running(&self) {
        let mut inner = self.lock();
        if matches!(inner.state, HlsState::Starting | HlsState::Running) {
            inner.state = HlsState::Running;
            inner.error = None;
        }
    }

    fn set_failed(&self, message: impl Into<String>) {
        let mut inner = self.lock();
        inner.state = HlsState::Failed;
        inner.error = Some(bounded(message.into()));
    }

    fn set_stopped(&self, message: impl Into<String>) {
        let mut inner = self.lock();
        if inner.state != HlsState::Failed {
            inner.state = HlsState::Stopped;
            inner.error = Some(bounded(message.into()));
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> HlsState {
        self.lock().state
    }

    /// Controller-facing status token.
    ///
    /// `running` is only returned while the playlist actually exists. A
    /// configured pipeline that has not written its manifest is reported as
    /// `failed` with an explanatory error, which keeps the endpoint honest.
    pub fn status_name(&self) -> &'static str {
        let inner = self.lock();
        match inner.state {
            HlsState::NotConfigured => "not-configured",
            HlsState::Starting | HlsState::Running => {
                if manifest_exists(&inner) {
                    "running"
                } else {
                    "failed"
                }
            }
            HlsState::Failed | HlsState::Stopped => "failed",
        }
    }

    /// Bounded error/status string for `/api/v1/connection`.
    pub fn error(&self) -> Option<String> {
        let inner = self.lock();
        match inner.state {
            HlsState::NotConfigured => None,
            HlsState::Starting | HlsState::Running => {
                if manifest_exists(&inner) {
                    inner.error.clone()
                } else {
                    Some(
                        inner
                            .manifest
                            .as_deref()
                            .map(|path| {
                                bounded(format!(
                                    "HLS manifest `{}` has not been written yet",
                                    path.display()
                                ))
                            })
                            .unwrap_or_else(|| {
                                bounded("HLS manifest has not been written yet".to_owned())
                            }),
                    )
                }
            }
            HlsState::Failed | HlsState::Stopped => Some(
                inner
                    .error
                    .clone()
                    .unwrap_or_else(|| "HLS pipeline is not running".to_owned()),
            ),
        }
    }

    /// Manifest this status tracks, when the pipeline was configured.
    pub fn manifest_path(&self) -> Option<PathBuf> {
        self.lock().manifest.clone()
    }
}

fn manifest_exists(inner: &HlsStatusInner) -> bool {
    inner
        .manifest
        .as_deref()
        .is_some_and(|manifest| manifest.is_file())
}

fn bounded(message: String) -> String {
    if message.chars().count() <= MAX_STATUS_ERROR_CHARS {
        return message;
    }
    let mut truncated: String = message.chars().take(MAX_STATUS_ERROR_CHARS).collect();
    truncated.push('…');
    truncated
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Monotonic clock used by [`FramePacer`].
///
/// Production uses [`SystemClock`]. Tests inject a manual clock so pacing is
/// asserted from synthetic timestamps without sleeping.
pub trait HlsClock: Send + Sync {
    /// Current monotonic time.
    fn now(&self) -> Instant;
}

/// The real monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl HlsClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Wall-clock pacer for the raw-frame feeder.
///
/// ffmpeg is told the input is `-r fps`, so every frame written to its stdin
/// advances media time by exactly `1 / fps`. A capture source can produce
/// frames much faster than that (the QMP display polls its own ceiling and can
/// run roughly six times faster than a 5 fps HLS timebase), which makes the
/// playlist's media time run ahead of wall time. This pacer drops frames that
/// arrive before their slot and admits at most `fps` frames per wall-clock
/// second.
///
/// The first frame is admitted immediately: `start` is the initial deadline.
/// After a written frame the deadline advances by one interval. A late frame
/// is still written, but a schedule that fell more than one interval behind is
/// reset to `now + interval` instead of banking a burst of catch-up frames.
#[derive(Clone, Debug)]
pub struct FramePacer {
    interval: Duration,
    next_deadline: Instant,
    emitted: u64,
    dropped: u64,
}

impl FramePacer {
    /// Create a pacer for `fps` whose first frame is due at `start`.
    ///
    /// `fps` is clamped to at least one. [`HlsConfig::validate`] already
    /// rejects zero before a pipeline starts; the clamp protects direct
    /// callers of the pacer.
    pub fn new(fps: u32, start: Instant) -> Self {
        Self {
            interval: Self::interval_for(fps),
            next_deadline: start,
            emitted: 0,
            dropped: 0,
        }
    }

    /// One frame interval for `fps`, at least one nanosecond.
    pub fn interval_for(fps: u32) -> Duration {
        let nanos = 1_000_000_000u64 / u64::from(fps.max(1));
        Duration::from_nanos(nanos.max(1))
    }

    /// Decide whether a frame observed at `now` should be written.
    ///
    /// Early frames are dropped without being written. A late frame is
    /// admitted; the following slot is one interval after `now` when the
    /// schedule fell further behind than that, so a stall cannot accumulate
    /// unbounded catch-up debt.
    pub fn should_emit(&mut self, now: Instant) -> bool {
        if now < self.next_deadline {
            self.dropped += 1;
            return false;
        }
        self.emitted += 1;
        self.next_deadline += self.interval;
        if self.next_deadline <= now {
            // The source or an ffmpeg write stalled. Do not bank a burst of
            // catch-up frames: the next slot is one interval from the frame
            // that was just written.
            self.next_deadline = now + self.interval;
        }
        true
    }

    /// Frames admitted by this pacer.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Frames dropped by this pacer.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Next scheduled slot.
    pub fn next_deadline(&self) -> Instant {
        self.next_deadline
    }

    /// Restart the pacing schedule after an encoder restart.
    ///
    /// The frame that triggered the restart has already been admitted, so the
    /// next slot is one interval after `now`. Any accumulated catch-up debt is
    /// discarded: a restart that took longer than an interval cannot make the
    /// feeder burst frames into the new ffmpeg child. Counters are preserved
    /// so status/evidence still describe the whole pipeline generation.
    pub fn rebase(&mut self, now: Instant) {
        self.next_deadline = now + self.interval;
    }
}

/// Destination for converted raw frames.
///
/// The ffmpeg child's stdin implements this; tests inject a recording sink so
/// the default suite never needs ffmpeg.
#[async_trait]
pub trait FrameSink: Send {
    /// Write one complete frame.
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Flush and close the sink. Called after end-of-stream or a failure.
    async fn close(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A spawned ffmpeg process plus the sink that writes to its stdin.
pub struct SpawnedFfmpeg {
    /// Raw-frame destination.
    pub sink: Box<dyn FrameSink>,
    /// Child process, when the spawner created one.
    pub child: Option<Child>,
}

/// Injectable ffmpeg process factory.
///
/// The default implementation runs ffmpeg; tests substitute a recording
/// spawner so argv construction, conversion, ordering, and shutdown are
/// asserted without the binary.
pub trait FfmpegSpawner: Send + Sync {
    /// Spawn ffmpeg for `width`×`height` raw RGB24 input.
    fn spawn(&self, config: &HlsConfig, width: u32, height: u32)
        -> Result<SpawnedFfmpeg, HlsError>;

    /// Spawn a ffmpeg generation that may be a restart after a frame-size
    /// change.
    ///
    /// `restart` is false for the first generation. A new generation must
    /// append to the existing playlist (`append_list`) and mark the switch as
    /// a discontinuity (`discont_start`) while still pruning old segments
    /// (`delete_segments`). The default delegates to [`Self::spawn`], so
    /// injected test/embedding spawners keep working unchanged; the system
    /// spawner overrides it to build the restart-aware argv.
    fn spawn_generation(
        &self,
        config: &HlsConfig,
        width: u32,
        height: u32,
        restart: bool,
    ) -> Result<SpawnedFfmpeg, HlsError> {
        let _ = restart;
        self.spawn(config, width, height)
    }
}

struct SystemFfmpegSpawner;

impl FfmpegSpawner for SystemFfmpegSpawner {
    fn spawn(
        &self,
        config: &HlsConfig,
        width: u32,
        height: u32,
    ) -> Result<SpawnedFfmpeg, HlsError> {
        self.spawn_generation(config, width, height, false)
    }

    fn spawn_generation(
        &self,
        config: &HlsConfig,
        width: u32,
        height: u32,
        restart: bool,
    ) -> Result<SpawnedFfmpeg, HlsError> {
        create_output_dir(config)?;
        let argv = ffmpeg_argv_for(config, width, height, restart);
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| HlsError::Spawn {
            bin: config.ffmpeg_bin.clone(),
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or(HlsError::StdinUnavailable)?;
        Ok(SpawnedFfmpeg {
            sink: Box::new(ChildStdinSink { stdin: Some(stdin) }),
            child: Some(child),
        })
    }
}

/// Encoder names ffmpeg advertised, parsed once from `ffmpeg -encoders`.
///
/// This is evidence, not a guess: the HLS path never starts a hardware encoder
/// just because its name was requested. Tests inject a static list through
/// [`StaticEncoderProbe`]; production runs [`SystemEncoderProbe`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EncoderAvailability {
    encoders: BTreeSet<String>,
}

impl EncoderAvailability {
    /// Build availability from exact ffmpeg encoder names.
    pub fn from_names(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            encoders: names.into_iter().map(Into::into).collect(),
        }
    }

    /// The software encoders this pipeline knows how to run.
    pub fn software() -> Self {
        Self::from_names(
            [
                HlsEncoder::H264,
                HlsEncoder::Hevc,
                HlsEncoder::Av1Svt,
                HlsEncoder::Av1Aom,
                HlsEncoder::Mjpeg,
            ]
            .map(HlsEncoder::ffmpeg_name),
        )
    }

    /// True when ffmpeg advertised this encoder name.
    pub fn contains(&self, encoder: HlsEncoder) -> bool {
        self.encoders.contains(encoder.ffmpeg_name())
    }

    /// True when ffmpeg advertised `name`.
    pub fn contains_name(&self, name: &str) -> bool {
        self.encoders.contains(name)
    }

    /// Encoder names known to this pipeline that ffmpeg advertised.
    pub fn known_encoders(&self) -> Vec<String> {
        HlsEncoder::ALL
            .iter()
            .filter(|encoder| self.contains(**encoder))
            .map(|encoder| encoder.ffmpeg_name().to_owned())
            .collect()
    }

    /// Parse `ffmpeg -hide_banner -encoders` output.
    ///
    /// The table starts after a dashed separator; each row is
    /// `<flags> <name> <description>`. Lines outside the table are ignored, so
    /// a partial or unexpected stderr/stdout cannot invent an encoder.
    pub fn parse_ffmpeg_encoders(output: &str) -> Self {
        let mut encoders = BTreeSet::new();
        let mut in_table = false;
        for line in output.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("------") {
                in_table = true;
                continue;
            }
            if !in_table {
                continue;
            }
            let mut fields = trimmed.split_whitespace();
            let flags = fields.next().unwrap_or_default();
            if flags.len() < 5 {
                continue;
            }
            if let Some(name) = fields.next() {
                encoders.insert(name.to_owned());
            }
        }
        Self { encoders }
    }
}

impl FromIterator<String> for EncoderAvailability {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self {
            encoders: iter.into_iter().collect(),
        }
    }
}

/// Injectable encoder availability check.
///
/// A probe returns evidence; it never starts an encoder. The default
/// implementation runs `ffmpeg -encoders` once (bounded and cached), while
/// tests use [`StaticEncoderProbe`] so no real ffmpeg or GPU is needed.
pub trait EncoderProbe: Send + Sync {
    /// ffmpeg encoders available to `ffmpeg_bin`.
    fn availability(&self, ffmpeg_bin: &Path) -> Result<EncoderAvailability, String>;

    /// Optional runtime evidence for a hardware encoder.
    ///
    /// `ffmpeg -encoders` proves only that a wrapper was compiled in; it does
    /// not prove that a device, driver, or rate-control combination works on
    /// this host. Production probes override this to run the exact HLS argv on
    /// a short synthetic frame and require a non-empty segment. The default
    /// succeeds so deterministic test probes do not need a GPU.
    fn runtime_check(&self, _config: &HlsConfig) -> Result<(), String> {
        Ok(())
    }
}

/// A deterministic probe over a fixed set of encoder names.
#[derive(Clone, Debug, Default)]
pub struct StaticEncoderProbe {
    availability: EncoderAvailability,
}

impl StaticEncoderProbe {
    /// A probe backed by an explicit availability set.
    pub fn new(availability: EncoderAvailability) -> Self {
        Self { availability }
    }

    /// A probe that knows only the software encoders (the conservative test
    /// default).
    pub fn software() -> Self {
        Self::new(EncoderAvailability::software())
    }

    /// A probe that advertises exactly these encoders.
    pub fn with_encoders(encoders: impl IntoIterator<Item = HlsEncoder>) -> Self {
        Self::new(EncoderAvailability::from_names(
            encoders.into_iter().map(HlsEncoder::ffmpeg_name),
        ))
    }
}

impl EncoderProbe for StaticEncoderProbe {
    fn availability(&self, _ffmpeg_bin: &Path) -> Result<EncoderAvailability, String> {
        Ok(self.availability.clone())
    }
}

const ENCODER_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ENCODER_PROBE_BYTES: u64 = 1024 * 1024;

/// The production probe: run `ffmpeg -hide_banner -loglevel error -encoders`
/// with a bounded wait and parse the table once.
#[derive(Debug, Default)]
pub struct SystemEncoderProbe {
    cached: Mutex<Option<EncoderAvailability>>,
}

impl SystemEncoderProbe {
    /// A fresh uncached probe.
    pub fn new() -> Self {
        Self::default()
    }
}

impl EncoderProbe for SystemEncoderProbe {
    fn availability(&self, ffmpeg_bin: &Path) -> Result<EncoderAvailability, String> {
        if let Some(cached) = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Ok(cached);
        }

        let mut child = std::process::Command::new(ffmpeg_bin)
            .args(["-hide_banner", "-loglevel", "error", "-encoders"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not execute `{}`: {error}", ffmpeg_bin.display()))?;
        let Some(mut stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`{}` did not expose stdout for -encoders",
                ffmpeg_bin.display()
            ));
        };

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout
                .by_ref()
                .take(MAX_ENCODER_PROBE_BYTES)
                .read_to_string(&mut text);
            let _ = sender.send(text);
        });

        let text = match receiver.recv_timeout(ENCODER_PROBE_TIMEOUT) {
            Ok(text) => text,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`{} -encoders` did not finish within {}s",
                    ffmpeg_bin.display(),
                    ENCODER_PROBE_TIMEOUT.as_secs()
                ));
            }
        };
        let _ = child.wait();

        let availability = EncoderAvailability::parse_ffmpeg_encoders(&text);
        *self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(availability.clone());
        Ok(availability)
    }

    fn runtime_check(&self, config: &HlsConfig) -> Result<(), String> {
        if !config.video_encoder.is_hardware() {
            return Ok(());
        }
        probe_hardware_encode(config)
    }
}

/// Upper bound on a real hardware encoder probe.
const HARDWARE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Stderr retained from a failed probe (the tail carries the real cause).
const HARDWARE_PROBE_STDERR_BYTES: usize = 4096;
/// Upper bound on stderr read before keeping its tail.
const HARDWARE_PROBE_STDERR_READ: u64 = 64 * 1024;

/// Encode three synthetic RGB24 frames with the exact HLS argv and require a
/// non-empty segment.
///
/// This is deliberately narrower than a full pipeline: it proves that ffmpeg
/// can initialise the encoder on the configured device and write a segment.
/// It runs in a throwaway directory and kills the child on timeout.
fn probe_hardware_encode(config: &HlsConfig) -> Result<(), String> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let directory = std::env::temp_dir().join(format!(
        "ozma-hls-encoder-probe-{}-{sequence}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "could not create probe directory `{}`: {error}",
            directory.display()
        )
    })?;

    let (width, height) = (320, 240);
    let probe_config = HlsConfig {
        output_dir: directory.clone(),
        fps: config.fps.clamp(1, 30),
        segment_seconds: 1.0,
        playlist_size: 4,
        width: None,
        height: None,
        ..config.clone()
    };
    let mut argv = ffmpeg_argv(&probe_config, width, height);
    // Use a real rawvideo file rather than a stdin pipe. A pipe write can block
    // indefinitely if the encoder never starts reading; the file keeps the
    // whole probe bounded by the process timeout.
    let raw_path = directory.join("probe.rgb");
    let mut frames = Vec::with_capacity(3 * (width as usize) * (height as usize) * 3);
    for index in 0..3 {
        frames.extend_from_slice(&probe_rgb24_frame(width, height, index));
    }
    std::fs::write(&raw_path, &frames)
        .map_err(|error| format!("could not write probe frames: {error}"))?;
    let input_index = argv
        .iter()
        .position(|argument| argument == "-i")
        .ok_or_else(|| "the HLS argv has no `-i` input".to_owned())?;
    if argv.get(input_index + 1).map(String::as_str) == Some("-") {
        argv[input_index + 1] = raw_path.display().to_string();
    }

    let stderr_path = directory.join("ffmpeg.stderr");
    let stderr_file = std::fs::File::create(&stderr_path)
        .map_err(|error| format!("could not create probe stderr file: {error}"))?;

    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|error| format!("could not execute `{}`: {error}", argv[0]))?;

    let deadline = Instant::now() + HARDWARE_PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = read_probe_stderr(&stderr_path);
                let _ = std::fs::remove_dir_all(&directory);
                return Err(format!(
                    "waiting for the probe encode failed: {error}; ffmpeg stderr: {stderr}"
                ));
            }
        }
    };

    let segment_written = std::fs::read_dir(&directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            let path = entry.path();
            path.extension() == Some(std::ffi::OsStr::new("ts"))
                && entry
                    .metadata()
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
        });
    let stderr = read_probe_stderr(&stderr_path);
    let _ = std::fs::remove_dir_all(&directory);

    match status {
        None => Err(format!(
            "the encoder probe did not finish within {}s; ffmpeg stderr: {stderr}",
            HARDWARE_PROBE_TIMEOUT.as_secs()
        )),
        Some(status) if !status.success() => {
            Err(format!("ffmpeg exited with {status}; stderr: {stderr}"))
        }
        Some(_) if !segment_written => Err(
            "ffmpeg exited cleanly but wrote no non-empty HLS segment; the encoder did not \
             produce output"
                .to_owned(),
        ),
        Some(_) => Ok(()),
    }
}

fn read_probe_stderr(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = file
        .take(HARDWARE_PROBE_STDERR_READ)
        .read_to_end(&mut bytes);
    if bytes.len() > HARDWARE_PROBE_STDERR_BYTES {
        bytes.drain(..bytes.len() - HARDWARE_PROBE_STDERR_BYTES);
    }
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

fn probe_rgb24_frame(width: u32, height: u32, index: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for y in 0..height {
        for x in 0..width {
            pixels.push(((x * 4 + index) & 0xFF) as u8);
            pixels.push(((y * 4) & 0xFF) as u8);
            pixels.push((((x + y) * 2 + index) & 0xFF) as u8);
        }
    }
    pixels
}

/// Check runtime-only requirements for the configured encoder.
///
/// VAAPI device existence is checked here, at start/restart, not in
/// [`ffmpeg_argv`]. Hardware availability comes from the injected probe; a
/// software encoder is not probed because its failure already surfaces as a
/// typed spawn error.
fn validate_encoder_runtime(config: &HlsConfig, probe: &dyn EncoderProbe) -> Result<(), HlsError> {
    let encoder = config.video_encoder;
    if encoder.is_vaapi() && !config.vaapi_device.exists() {
        return Err(HlsError::VaapiDevice {
            path: config.vaapi_device.clone(),
            message: "the path does not exist".to_owned(),
            remediation: format!(
                "Pass --vaapi-device with an existing render node (default \
                 /dev/dri/renderD128), or choose a software encoder."
            ),
        });
    }
    if !encoder.is_hardware() {
        return Ok(());
    }

    let availability =
        probe
            .availability(&config.ffmpeg_bin)
            .map_err(|message| HlsError::EncoderUnavailable {
                encoder: encoder.ffmpeg_name().to_owned(),
                backend: encoder.backend_label().to_owned(),
                message: format!("could not probe ffmpeg encoders: {message}"),
                remediation: format!(
                    "Check that `{}` can run and lists encoders, or choose a software encoder.",
                    config.ffmpeg_bin.display()
                ),
                available: Vec::new(),
            })?;
    if !availability.contains(encoder) {
        return Err(HlsError::EncoderUnavailable {
            encoder: encoder.ffmpeg_name().to_owned(),
            backend: encoder.backend_label().to_owned(),
            message: format!("ffmpeg does not advertise `{}`", encoder.ffmpeg_name()),
            remediation: format!(
                "Install or enable the {} encoder for ffmpeg and the matching driver, choose \
                 another hardware backend, or explicitly allow a software fallback.",
                encoder.backend_label()
            ),
            available: availability.known_encoders(),
        });
    }

    // A compiled-in wrapper is not proof that this host can encode. Run the
    // exact argv on a short synthetic frame; a missing or unusable device must
    // fail here, before a pipeline is reported as started/applied.
    probe
        .runtime_check(config)
        .map_err(|message| HlsError::EncoderUnavailable {
            encoder: encoder.ffmpeg_name().to_owned(),
            backend: encoder.backend_label().to_owned(),
            message: format!(
                "the {} encoder failed a real encode probe: {message}",
                encoder.ffmpeg_name()
            ),
            remediation: format!(
            "Fix the {} device or driver (for example the VAAPI render node, NVENC/QSV device, \
             or V4L2M2M node), choose another hardware backend, or explicitly allow a software \
             fallback.",
            encoder.backend_label()
        ),
            // Other hardware wrappers were only advertised, not runtime-proven.
            // Listing them as "available" would repeat the misleading claim
            // this probe exists to prevent, so report only software encoders.
            available: availability
                .known_encoders()
                .into_iter()
                .filter(|name| {
                    HlsEncoder::parse(name)
                        .map(|candidate| !candidate.is_hardware())
                        .unwrap_or(false)
                })
                .collect(),
        })?;
    Ok(())
}

struct ChildStdinSink {
    stdin: Option<ChildStdin>,
}

#[async_trait]
impl FrameSink for ChildStdinSink {
    async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "ffmpeg stdin is already closed")
        })?;
        stdin.write_all(bytes).await
    }

    async fn close(&mut self) -> io::Result<()> {
        // Dropping stdin sends EOF, letting ffmpeg flush the final segment and
        // playlist entry.
        self.stdin.take();
        Ok(())
    }
}

/// A running HLS pipeline.
pub struct HlsPipeline {
    config: HlsConfig,
    manifest: PathBuf,
    status: HlsStatus,
    task: Option<JoinHandle<()>>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<Notify>,
    stopped: bool,
}

impl HlsPipeline {
    /// Spawn ffmpeg and start consuming `source`.
    ///
    /// When both the source's declared dimensions and the config override are
    /// absent (QMP display reports `0x0`), ffmpeg is spawned on the first frame
    /// once its real size is known.
    pub fn start(source: Box<dyn CaptureSource>, config: HlsConfig) -> Result<Self, HlsError> {
        Self::start_with_spawner(source, config, Arc::new(SystemFfmpegSpawner))
    }

    /// Start with an injected process factory, for tests and embedding.
    ///
    /// The conservative probe knows only the software encoders, so a hardware
    /// request on this constructor fails with a typed
    /// [`HlsError::EncoderUnavailable`] instead of assuming the name works.
    pub fn start_with_spawner(
        source: Box<dyn CaptureSource>,
        config: HlsConfig,
        spawner: Arc<dyn FfmpegSpawner>,
    ) -> Result<Self, HlsError> {
        Self::start_with_spawner_and_probe(
            source,
            config,
            spawner,
            Arc::new(StaticEncoderProbe::software()),
        )
    }

    /// Start with an injected process factory and encoder probe.
    ///
    /// Production uses [`SystemEncoderProbe`]; tests inject a static list so
    /// no real ffmpeg or GPU is required. Pacing uses [`SystemClock`]; use the
    /// crate-internal clock-aware constructor for deterministic tests.
    pub fn start_with_spawner_and_probe(
        source: Box<dyn CaptureSource>,
        config: HlsConfig,
        spawner: Arc<dyn FfmpegSpawner>,
        probe: Arc<dyn EncoderProbe>,
    ) -> Result<Self, HlsError> {
        Self::start_with_clock(source, config, spawner, probe, Arc::new(SystemClock))
    }

    /// Start with an injected process factory, encoder probe, and clock.
    fn start_with_clock(
        source: Box<dyn CaptureSource>,
        config: HlsConfig,
        spawner: Arc<dyn FfmpegSpawner>,
        probe: Arc<dyn EncoderProbe>,
        clock: Arc<dyn HlsClock>,
    ) -> Result<Self, HlsError> {
        let config = config.validated()?;
        validate_encoder_runtime(&config, probe.as_ref())?;
        if let Err(error) = ensure_supported(source.format().pixel_format) {
            return Err(error);
        }
        create_output_dir(&config)?;

        let manifest = config.manifest_path();
        let status = HlsStatus::starting(manifest.clone());
        let declared = source.format();
        let expected = initial_dimensions(&config, declared);

        let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
        let sink = match expected {
            Some((width, height)) => match spawner.spawn(&config, width, height) {
                Ok(spawned) => {
                    *child_slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = spawned.child;
                    Some(spawned.sink)
                }
                Err(error) => {
                    status.set_failed(error.to_string());
                    return Err(error);
                }
            },
            None => None,
        };

        let cancel = Arc::new(Notify::new());
        let task = tokio::spawn(run_pipeline(
            source,
            config.clone(),
            declared,
            expected,
            spawner,
            status.clone(),
            child_slot.clone(),
            cancel.clone(),
            sink,
            clock,
        ));

        Ok(Self {
            config,
            manifest,
            status,
            task: Some(task),
            child: child_slot,
            cancel,
            stopped: false,
        })
    }

    /// True while the frame task is alive and has not been stopped.
    pub fn is_running(&self) -> bool {
        !self.stopped && matches!(self.status.state(), HlsState::Starting | HlsState::Running)
    }

    /// Playlist the controller proxies.
    pub fn manifest_path(&self) -> &Path {
        &self.manifest
    }

    /// Directory containing the playlist and segments.
    pub fn output_dir(&self) -> &Path {
        &self.config.output_dir
    }

    /// Configuration this pipeline runs with.
    pub fn config(&self) -> &HlsConfig {
        &self.config
    }

    /// Cloneable status for `/api/v1/connection`.
    pub fn status(&self) -> HlsStatus {
        self.status.clone()
    }

    /// Kill ffmpeg, abort and await the frame task, and stop the source.
    ///
    /// Idempotent: later calls are no-ops.
    pub async fn stop(&mut self) -> Result<(), HlsError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        kill_child(&self.child).await;
        // Ask the task to leave its frame wait so the source's `stop()` runs
        // and the device/socket is released. `notify_one` stores a permit, so a
        // stop that races the loop cannot be lost.
        self.cancel.notify_one();

        let mut task_error = None;
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(2), &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_cancelled() => {}
                Ok(Err(error)) => task_error = Some(error.to_string()),
                Err(_) => {
                    // A source that ignores cancellation cannot hold shutdown.
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        // A lazily spawned child may have been stored between the first kill
        // and the task abort.
        kill_child(&self.child).await;

        if let Some(message) = task_error {
            self.status
                .set_failed(format!("HLS frame task failed: {message}"));
            return Err(HlsError::Task { message });
        }
        self.status.set_stopped("HLS pipeline stopped");
        Ok(())
    }
}

impl Drop for HlsPipeline {
    fn drop(&mut self) {
        self.cancel.notify_one();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        // The child was spawned with `kill_on_drop`, so dropping the handle is
        // enough; this also covers a lazily spawned child.
        if let Ok(mut slot) = self.child.lock() {
            slot.take();
        }
    }
}

/// Factory that opens a fresh capture source for a new pipeline generation.
///
/// A restart must not reuse a source that the old frame task owned: capture
/// backends release their device/socket in `stop()`, so a new source is opened
/// from the same registry after the old pipeline has stopped.
pub type HlsSourceFactory = Arc<dyn Fn() -> Result<Box<dyn CaptureSource>, HlsError> + Send + Sync>;

/// A hardware restart that was explicitly allowed to use software instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HlsEncoderFallback {
    /// Hardware encoder the caller asked for.
    pub requested: HlsEncoder,
    /// Software encoder actually applied.
    pub applied: HlsEncoder,
    /// Why the hardware encoder could not start.
    pub reason: String,
}

/// Result of a successful HLS restart.
#[derive(Clone, Debug)]
pub struct HlsRestartOutcome {
    /// Live pipeline status.
    pub status: HlsStatus,
    /// Encoder the caller requested.
    pub requested_encoder: HlsEncoder,
    /// Encoder the new pipeline actually runs.
    pub applied_encoder: HlsEncoder,
    /// Present only when an explicit software fallback was used.
    pub fallback: Option<HlsEncoderFallback>,
}

/// Owns the current [`HlsPipeline`] so it can be stopped and restarted.
///
/// `POST /codec` uses [`HlsController::restart_with_encoder`] to apply a codec
/// override to the running HLS stream. The controller is cloneable; every
/// clone shares the same pipeline slot, configuration, and status.
#[derive(Clone)]
pub struct HlsController {
    pipeline: Arc<tokio::sync::Mutex<Option<HlsPipeline>>>,
    config: Arc<Mutex<HlsConfig>>,
    status: Arc<Mutex<HlsStatus>>,
    source_factory: HlsSourceFactory,
    spawner: Arc<dyn FfmpegSpawner>,
    probe: Arc<dyn EncoderProbe>,
}

impl HlsController {
    /// Build a controller that spawns the system ffmpeg for each generation.
    ///
    /// Hardware encoder requests are checked against `ffmpeg -encoders` before
    /// any pipeline starts; nothing is assumed from the encoder name.
    pub fn new(source_factory: HlsSourceFactory, config: HlsConfig) -> Self {
        Self::with_spawner_and_probe(
            source_factory,
            config,
            Arc::new(SystemFfmpegSpawner),
            Arc::new(SystemEncoderProbe::new()),
        )
    }

    /// Build a controller with an injected process factory, for tests.
    ///
    /// The default probe knows only software encoders; tests that exercise a
    /// hardware restart must inject an explicit probe with
    /// [`Self::with_spawner_and_probe`].
    pub fn with_spawner(
        source_factory: HlsSourceFactory,
        config: HlsConfig,
        spawner: Arc<dyn FfmpegSpawner>,
    ) -> Self {
        Self::with_spawner_and_probe(
            source_factory,
            config,
            spawner,
            Arc::new(StaticEncoderProbe::software()),
        )
    }

    /// Build a controller with an injected process factory and encoder probe.
    pub fn with_spawner_and_probe(
        source_factory: HlsSourceFactory,
        config: HlsConfig,
        spawner: Arc<dyn FfmpegSpawner>,
        probe: Arc<dyn EncoderProbe>,
    ) -> Self {
        Self {
            pipeline: Arc::new(tokio::sync::Mutex::new(None)),
            config: Arc::new(Mutex::new(config)),
            status: Arc::new(Mutex::new(HlsStatus::not_configured())),
            source_factory,
            spawner,
            probe,
        }
    }

    /// Configuration of the current (or next) pipeline generation.
    pub fn config(&self) -> HlsConfig {
        lock(&self.config).clone()
    }

    /// Cloneable status of the current pipeline generation.
    pub fn status(&self) -> HlsStatus {
        lock(&self.status).clone()
    }

    /// True while the current pipeline is starting or running.
    pub fn is_running(&self) -> bool {
        matches!(
            self.status().state(),
            HlsState::Starting | HlsState::Running
        )
    }

    /// Open a source and start a pipeline without touching controller state.
    fn open_and_start(&self, config: HlsConfig) -> Result<HlsPipeline, HlsError> {
        let source = (self.source_factory)()?;
        HlsPipeline::start_with_spawner_and_probe(
            source,
            config,
            self.spawner.clone(),
            self.probe.clone(),
        )
    }

    /// Start the pipeline if it is not already running.
    ///
    /// A source or spawn failure returns a typed error and stores `failed`
    /// status; no success is claimed.
    pub async fn start(&self) -> Result<HlsStatus, HlsError> {
        let mut pipeline = self.pipeline.lock().await;
        if pipeline.is_some() {
            return Ok(self.status());
        }
        let config = self.config();
        match self.open_and_start(config) {
            Ok(started) => {
                let status = started.status();
                *lock(&self.status) = status.clone();
                *pipeline = Some(started);
                Ok(status)
            }
            Err(error) => {
                *lock(&self.status) = HlsStatus::failed(error.to_string());
                Err(error)
            }
        }
    }

    /// Stop and drop the current pipeline.
    ///
    /// Idempotent when no pipeline is running. The status is updated from the
    /// pipeline's final state (normally `stopped`/`failed`).
    pub async fn stop(&self) -> Result<(), HlsError> {
        let mut pipeline = self.pipeline.lock().await;
        let Some(mut current) = pipeline.take() else {
            return Ok(());
        };
        let result = current.stop().await;
        *lock(&self.status) = current.status();
        result
    }

    /// Restart HLS with a different ffmpeg encoder.
    ///
    /// Hardware encoders are probed first. No software substitution happens on
    /// this method; use [`Self::restart_with_encoder_fallback`] when a caller
    /// has explicitly allowed a fallback.
    ///
    /// The old pipeline is stopped before the new source is opened. If the new
    /// generation cannot start, the previous configuration is restored with a
    /// fresh source. When restoration also fails, HLS is left stopped and the
    /// status/error reports both failures rather than claiming success.
    pub async fn restart_with_encoder(
        &self,
        encoder: HlsEncoder,
        bitrate_kbps: Option<u32>,
    ) -> Result<HlsStatus, HlsError> {
        self.restart_with_encoder_fallback(encoder, bitrate_kbps, false)
            .await
            .map(|outcome| outcome.status)
    }

    /// Restart HLS with an optional explicit software fallback.
    ///
    /// When `allow_software_fallback` is true and the requested hardware
    /// encoder is unavailable (or its VAAPI device is missing), the matching
    /// software encoder is started and the returned
    /// [`HlsRestartOutcome::fallback`] records exactly what happened. Without
    /// that opt-in the typed availability error is returned and the previous
    /// pipeline is restored when possible.
    pub async fn restart_with_encoder_fallback(
        &self,
        encoder: HlsEncoder,
        bitrate_kbps: Option<u32>,
        allow_software_fallback: bool,
    ) -> Result<HlsRestartOutcome, HlsError> {
        let mut pipeline = self.pipeline.lock().await;
        let Some(mut previous) = pipeline.take() else {
            return Err(HlsError::NotRunning);
        };

        let previous_config = self.config();
        // Stop before opening a second source: capture devices are exclusive and
        // the old frame task owns the current one.
        let _ = previous.stop().await;

        let mut next_config = previous_config.clone();
        next_config.video_encoder = encoder;
        if let Some(bitrate) = bitrate_kbps {
            next_config.bitrate_kbps = bitrate;
        }

        match self.open_and_start(next_config.clone()) {
            Ok(started) => {
                *lock(&self.config) = next_config;
                let status = started.status();
                *lock(&self.status) = status.clone();
                *pipeline = Some(started);
                Ok(HlsRestartOutcome {
                    status,
                    requested_encoder: encoder,
                    applied_encoder: encoder,
                    fallback: None,
                })
            }
            Err(start_error) => {
                let start_message = start_error.to_string();
                if allow_software_fallback && is_encoder_availability_error(&start_error) {
                    if let Some(fallback) = encoder.software_fallback() {
                        let mut fallback_config = next_config.clone();
                        fallback_config.video_encoder = fallback;
                        match self.open_and_start(fallback_config.clone()) {
                            Ok(started) => {
                                *lock(&self.config) = fallback_config;
                                let status = started.status();
                                *lock(&self.status) = status.clone();
                                *pipeline = Some(started);
                                return Ok(HlsRestartOutcome {
                                    status,
                                    requested_encoder: encoder,
                                    applied_encoder: fallback,
                                    fallback: Some(HlsEncoderFallback {
                                        requested: encoder,
                                        applied: fallback,
                                        reason: start_message,
                                    }),
                                });
                            }
                            Err(fallback_error) => {
                                let fallback_message = format!(
                                    "{start_message}; software fallback `{}` also failed: \
                                     {fallback_error}",
                                    fallback.ffmpeg_name()
                                );
                                return self
                                    .restore_previous(
                                        &mut pipeline,
                                        previous_config,
                                        HlsError::Restart {
                                            message: fallback_message,
                                        },
                                    )
                                    .await;
                            }
                        }
                    }
                }

                self.restore_previous(&mut pipeline, previous_config, start_error)
                    .await
            }
        }
    }

    /// Restore the previous generation after a failed restart.
    async fn restore_previous(
        &self,
        pipeline: &mut Option<HlsPipeline>,
        previous_config: HlsConfig,
        start_error: HlsError,
    ) -> Result<HlsRestartOutcome, HlsError> {
        let start_message = start_error.to_string();
        match self.open_and_start(previous_config.clone()) {
            Ok(restored) => {
                let status = restored.status();
                *lock(&self.status) = status.clone();
                *pipeline = Some(restored);
                Err(augment_restored_error(
                    start_error,
                    format!(
                        "; restored the previous pipeline with encoder `{}`",
                        previous_config.video_encoder.ffmpeg_name()
                    ),
                ))
            }
            Err(restore_error) => {
                let message = format!(
                    "{start_message}; restoring the previous pipeline with encoder `{}` also \
                     failed: {restore_error}",
                    previous_config.video_encoder.ffmpeg_name()
                );
                *lock(&self.status) = HlsStatus::failed(message.clone());
                Err(HlsError::Restart { message })
            }
        }
    }
}

fn is_encoder_availability_error(error: &HlsError) -> bool {
    matches!(
        error,
        HlsError::EncoderUnavailable { .. } | HlsError::VaapiDevice { .. }
    )
}

/// Keep the typed availability error while noting that the old pipeline was
/// restored, instead of flattening it into a generic restart error.
fn augment_restored_error(error: HlsError, suffix: String) -> HlsError {
    match error {
        HlsError::EncoderUnavailable {
            encoder,
            backend,
            message,
            remediation,
            available,
        } => HlsError::EncoderUnavailable {
            encoder,
            backend,
            message: format!("{message}{suffix}"),
            remediation,
            available,
        },
        HlsError::VaapiDevice {
            path,
            message,
            remediation,
        } => HlsError::VaapiDevice {
            path,
            message: format!("{message}{suffix}"),
            remediation,
        },
        other => HlsError::Restart {
            message: format!("{other}{suffix}"),
        },
    }
}

async fn kill_child(child: &Arc<Mutex<Option<Child>>>) {
    let child = child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(mut child) = child {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    mut source: Box<dyn CaptureSource>,
    config: HlsConfig,
    declared: FrameFormat,
    mut expected: Option<(u32, u32)>,
    spawner: Arc<dyn FfmpegSpawner>,
    status: HlsStatus,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<Notify>,
    mut sink: Option<Box<dyn FrameSink>>,
    clock: Arc<dyn HlsClock>,
) {
    let mut pacer = FramePacer::new(config.fps, clock.now());
    let mut wrote_frame = false;
    loop {
        let frame = match tokio::select! {
            biased;
            _ = cancel.notified() => {
                status.set_stopped("HLS pipeline stopped");
                break;
            }
            frame = source.next_frame() => frame,
        } {
            Ok(frame) => frame,
            Err(MediaError::EndOfStream) => {
                status.set_stopped("capture source reached the end of its stream");
                break;
            }
            Err(error) => {
                status.set_failed(format!("capture source `{}` failed: {error}", source.id()));
                break;
            }
        };

        // ffmpeg's input timebase is `-r fps`, so only write frames that are
        // due. Dropping here keeps playlist media time close to wall time and
        // avoids converting a frame ffmpeg will never see.
        if !pacer.should_emit(clock.now()) {
            continue;
        }

        if frame.format().pixel_format != declared.pixel_format {
            status.set_failed(format!(
                "frame format `{}` does not match the source's declared `{}` format",
                frame.format().pixel_format,
                declared.pixel_format
            ));
            break;
        }

        let (width, height) = match expected {
            Some((expected_width, expected_height)) => {
                let frame_width = frame.format().width;
                let frame_height = frame.format().height;
                if frame_width != expected_width || frame_height != expected_height {
                    if !dimensions_are_dynamic(&config) {
                        // Pinned dimensions are an explicit contract. Scaling is
                        // deliberately not silent: a frame of another size is a
                        // visible configuration failure, not a blurry stream.
                        let error = HlsError::FrameDimensions {
                            width: frame_width,
                            height: frame_height,
                            expected_width,
                            expected_height,
                        };
                        status.set_failed(error.to_string());
                        break;
                    }
                    if frame_width == 0 || frame_height == 0 {
                        status.set_failed("capture frame has zero dimensions");
                        break;
                    }

                    // The old child was started with the old `-s WxH`. Close
                    // its stdin so a partial segment can be flushed, then reap
                    // it before spawning the new generation. A dead child is
                    // never fed again: if the restart fails the pipeline ends
                    // as `failed`.
                    if let Some(mut writer) = sink.take() {
                        let _ = writer.close().await;
                    }
                    kill_child(&child).await;
                    match spawner.spawn_generation(&config, frame_width, frame_height, true) {
                        Ok(spawned) => {
                            *child
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = spawned.child;
                            sink = Some(spawned.sink);
                        }
                        Err(error) => {
                            let error = HlsError::Restart {
                                message: format!(
                                    "ffmpeg could not restart for a frame-size change \
                                     {expected_width}x{expected_height} -> \
                                     {frame_width}x{frame_height}: {error}"
                                ),
                            };
                            status.set_failed(error.to_string());
                            break;
                        }
                    }
                    expected = Some((frame_width, frame_height));
                    // The frame that triggered the restart is already admitted.
                    // Rebase so a slow restart cannot make the feeder burst a
                    // backlog of frames into the new ffmpeg child.
                    pacer.rebase(clock.now());
                }
                (frame_width, frame_height)
            }
            None => {
                let width = frame.format().width;
                let height = frame.format().height;
                if width == 0 || height == 0 {
                    status.set_failed("capture frame has zero dimensions");
                    break;
                }
                match spawner.spawn_generation(&config, width, height, false) {
                    Ok(spawned) => {
                        *child
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = spawned.child;
                        sink = Some(spawned.sink);
                    }
                    Err(error) => {
                        status.set_failed(error.to_string());
                        break;
                    }
                }
                expected = Some((width, height));
                // Starting ffmpeg can take longer than one frame interval.
                // Rebase from the completed spawn rather than the frame's
                // arrival time so the first second is not a catch-up burst.
                pacer.rebase(clock.now());
                (width, height)
            }
        };

        let bytes = match convert_frame(&frame, declared.pixel_format, width, height) {
            Ok(bytes) => bytes,
            Err(error) => {
                status.set_failed(error.to_string());
                break;
            }
        };

        let Some(writer) = sink.as_mut() else {
            status.set_failed("ffmpeg input is not available");
            break;
        };
        if let Err(error) = writer.write(&bytes).await {
            status.set_failed(format!("writing a frame to ffmpeg failed: {error}"));
            break;
        }
        if !wrote_frame {
            wrote_frame = true;
            status.set_running();
        }
    }

    if let Some(writer) = sink.as_mut() {
        let _ = writer.close().await;
    }
    let _ = source.stop().await;
    if !wrote_frame && status.state() == HlsState::Starting {
        status.set_failed("HLS pipeline ended before writing a frame");
    }
}

/// Convert one frame into the packed RGB24 bytes ffmpeg expects.
fn convert_frame(
    frame: &Frame,
    expected_format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, HlsError> {
    let actual_format = frame.format().pixel_format;
    if actual_format != expected_format {
        return Err(HlsError::FormatMismatch {
            expected: expected_format,
            actual: actual_format,
        });
    }
    if frame.format().width != width || frame.format().height != height {
        return Err(HlsError::FrameDimensions {
            width: frame.format().width,
            height: frame.format().height,
            expected_width: width,
            expected_height: height,
        });
    }

    let data = frame.data();
    match expected_format {
        PixelFormat::Rgb24 => {
            let expected = byte_len(PixelFormat::Rgb24, width, height)?;
            if data.len() != expected {
                return Err(HlsError::FrameSize {
                    format: PixelFormat::Rgb24,
                    width,
                    height,
                    expected,
                    actual: data.len(),
                });
            }
            Ok(data.to_vec())
        }
        PixelFormat::Bgra => {
            let expected = byte_len(PixelFormat::Bgra, width, height)?;
            if data.len() != expected {
                return Err(HlsError::FrameSize {
                    format: PixelFormat::Bgra,
                    width,
                    height,
                    expected,
                    actual: data.len(),
                });
            }
            let mut rgb = Vec::with_capacity(expected / 4 * 3);
            for pixel in data.chunks_exact(4) {
                rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
            Ok(rgb)
        }
        other => Err(HlsError::UnsupportedFormat(other)),
    }
}

fn byte_len(format: PixelFormat, width: u32, height: u32) -> Result<usize, HlsError> {
    let pixels = u64::from(width) * u64::from(height);
    let bytes = match format {
        PixelFormat::Rgb24 => pixels * 3,
        PixelFormat::Bgra => pixels * 4,
        other => return Err(HlsError::UnsupportedFormat(other)),
    };
    usize::try_from(bytes).map_err(|_| {
        HlsError::InvalidConfig(format!(
            "frame dimensions {width}x{height} overflow a host buffer"
        ))
    })
}

fn ensure_supported(format: PixelFormat) -> Result<(), HlsError> {
    match format {
        PixelFormat::Rgb24 | PixelFormat::Bgra => Ok(()),
        other => Err(HlsError::UnsupportedFormat(other)),
    }
}

fn initial_dimensions(config: &HlsConfig, declared: FrameFormat) -> Option<(u32, u32)> {
    match (config.width, config.height) {
        (Some(width), Some(height)) => Some((width, height)),
        (None, None) if declared.width > 0 && declared.height > 0 => {
            Some((declared.width, declared.height))
        }
        (None, None) => None,
        _ => None, // rejected by HlsConfig::validate
    }
}

/// True when a later frame size may restart ffmpeg instead of failing.
///
/// Only a fully dynamic configuration may change size. Explicit
/// [`HlsConfig::width`]/[`HlsConfig::height`] pin the input contract;
/// [`HlsConfig::validate`] already rejects setting only one of them.
fn dimensions_are_dynamic(config: &HlsConfig) -> bool {
    config.width.is_none() && config.height.is_none()
}

fn create_output_dir(config: &HlsConfig) -> Result<(), HlsError> {
    std::fs::create_dir_all(&config.output_dir).map_err(|error| HlsError::OutputDirectory {
        path: config.output_dir.clone(),
        message: error.to_string(),
    })
}

/// Build the exact ffmpeg argument vector for `width`×`height` RGB24 input.
///
/// The binary itself is the first element. Tests assert this list directly; the
/// system spawner uses the same function, so argv and behavior cannot drift.
/// This is the first-generation argv; a restart generation additionally uses
/// [`ffmpeg_argv_for`] to append to the existing playlist.
pub fn ffmpeg_argv(config: &HlsConfig, width: u32, height: u32) -> Vec<String> {
    ffmpeg_argv_for(config, width, height, false)
}

/// Build the argv for a first run or for a restart after a frame-size change.
///
/// A restart keeps the same encoder, bitrate, segment duration, playlist size,
/// and output paths, but adds `append_list` (keep the existing playlist),
/// `discont_start` (mark the size change as an HLS discontinuity), and leaves
/// `delete_segments` on so the rolling window still prunes old segments. With
/// the default `-start_number 0`, ffmpeg continues segment file numbering from
/// the last filename in the appended playlist instead of overwriting it; that
/// was verified against ffmpeg n9.1 (`seg_00009.ts` was retained and followed
/// by `seg_00010.ts`/`seg_00011.ts`, media sequence 6 -> 9).
///
/// ffmpeg owns the encoded media timeline: each generation restarts it at zero,
/// and `discont_start` tells players not to interpolate across the boundary.
/// The feeder writes raw bytes only, so frame sequence/PTS order is unchanged.
fn ffmpeg_argv_for(config: &HlsConfig, width: u32, height: u32, restart: bool) -> Vec<String> {
    let bitrate = format!("{}k", config.bitrate_kbps);
    let mut argv = vec![
        config.ffmpeg_bin.display().to_string(),
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "warning".to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-pix_fmt".to_owned(),
        "rgb24".to_owned(),
        "-s".to_owned(),
        format!("{width}x{height}"),
        "-r".to_owned(),
        config.fps.to_string(),
        "-i".to_owned(),
        "-".to_owned(),
        "-an".to_owned(),
    ];

    match config.video_encoder {
        HlsEncoder::H264 => argv.extend([
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-preset".to_owned(),
            "veryfast".to_owned(),
            "-tune".to_owned(),
            "zerolatency".to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::Hevc => argv.extend([
            "-c:v".to_owned(),
            "libx265".to_owned(),
            "-preset".to_owned(),
            "ultrafast".to_owned(),
            "-tune".to_owned(),
            "zerolatency".to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::Av1Svt => argv.extend([
            "-c:v".to_owned(),
            "libsvtav1".to_owned(),
            "-preset".to_owned(),
            "10".to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::Av1Aom => argv.extend([
            "-c:v".to_owned(),
            "libaom-av1".to_owned(),
            "-cpu-used".to_owned(),
            "8".to_owned(),
            "-row-mt".to_owned(),
            "1".to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::Mjpeg => argv.extend([
            "-c:v".to_owned(),
            "mjpeg".to_owned(),
            "-q:v".to_owned(),
            "5".to_owned(),
        ]),
        HlsEncoder::VaapiH264 | HlsEncoder::VaapiHevc => argv.extend([
            "-vaapi_device".to_owned(),
            config.vaapi_device.display().to_string(),
            "-vf".to_owned(),
            "format=nv12,hwupload".to_owned(),
            "-c:v".to_owned(),
            config.video_encoder.ffmpeg_name().to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::NvencH264 | HlsEncoder::NvencHevc => argv.extend([
            "-c:v".to_owned(),
            config.video_encoder.ffmpeg_name().to_owned(),
            "-preset".to_owned(),
            "p4".to_owned(),
            "-tune".to_owned(),
            "ll".to_owned(),
            "-rc".to_owned(),
            "cbr".to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::QsvH264 | HlsEncoder::QsvHevc => argv.extend([
            "-c:v".to_owned(),
            config.video_encoder.ffmpeg_name().to_owned(),
            "-preset".to_owned(),
            "veryfast".to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
        HlsEncoder::V4l2m2mH264 | HlsEncoder::V4l2m2mHevc => argv.extend([
            "-c:v".to_owned(),
            config.video_encoder.ffmpeg_name().to_owned(),
            "-b:v".to_owned(),
            bitrate,
        ]),
    }

    // Pin the encoded chroma format. The raw input is RGB24, and without an
    // explicit target ffmpeg may choose 4:4:4 (libx264 yuv444p, NVENC gbrp),
    // which HLS/browser decoders commonly reject.
    if let Some(pixel_format) = config.video_encoder.hls_pixel_format() {
        argv.extend(["-pix_fmt".to_owned(), pixel_format.to_owned()]);
    }

    // Keep GOPs aligned with segment boundaries. `-hls_time` can only cut a
    // segment at a keyframe, so without this libx264's default 250-frame GOP
    // made `-hls_time 1` produce 50-second segments at 5 fps (see
    // docs/gate2-live-qmp-report.md, Finding 2). `-g` sets the fixed interval;
    // `-keyint_min`/`-sc_threshold 0` stop scene changes from drifting it on
    // software encoders; `-force_key_frames` closes a segment on time even
    // when fps × segment_seconds is fractional. Hardware wrappers do not all
    // honour the software-only flags, so they receive only `-g` and the force
    // expression.
    let gop = config.gop_frames();
    argv.extend(["-g".to_owned(), gop.to_string()]);
    if !config.video_encoder.is_hardware() {
        argv.extend([
            "-keyint_min".to_owned(),
            gop.to_string(),
            "-sc_threshold".to_owned(),
            "0".to_owned(),
        ]);
    }
    argv.extend([
        "-force_key_frames".to_owned(),
        config.force_key_frames_expr(),
    ]);

    argv.extend([
        "-f".to_owned(),
        "hls".to_owned(),
        "-hls_time".to_owned(),
        format_seconds(config.segment_seconds),
        "-hls_list_size".to_owned(),
        config.playlist_size.to_string(),
        // Remove segments that fall out of the rolling playlist. Without this
        // flag the output directory grows without bound. A restart keeps the
        // old entries and appends new ones instead of truncating the playlist;
        // `discont_start` marks the dimensions/timebase switch so players do
        // not decode across it as if it were continuous.
        "-hls_flags".to_owned(),
        if restart {
            "delete_segments+append_list+discont_start".to_owned()
        } else {
            "delete_segments".to_owned()
        },
        "-hls_segment_filename".to_owned(),
        config.segment_pattern().display().to_string(),
        config.manifest_path().display().to_string(),
    ]);
    argv
}

fn format_seconds(value: f32) -> String {
    let mut text = format!("{value:.3}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// Unauthenticated HLS file service, mounted at `/stream`.
///
/// `ServeDir` rejects `..` components, so a request cannot escape `output_dir`.
pub fn stream_router(output_dir: impl Into<PathBuf>) -> Router {
    Router::new().nest_service("/stream", ServeDir::new(output_dir.into()))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use ozma_media::Frame;

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("ozma-hls-{label}-{}-{serial}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create test directory");
        directory
    }

    fn frame(format: FrameFormat, sequence: u64, data: Vec<u8>) -> Frame {
        Frame::with_data(format, sequence * 1_000, sequence, true, data).expect("valid frame")
    }

    fn rgb_frame(width: u32, height: u32, sequence: u64, fill: u8) -> Frame {
        let format = FrameFormat::rgb24(width, height);
        frame(format, sequence, vec![fill; (width * height * 3) as usize])
    }

    struct FakeSource {
        format: FrameFormat,
        frames: VecDeque<Result<Frame, MediaError>>,
        hang_when_empty: bool,
        stopped: Arc<AtomicBool>,
        clock: Option<(Arc<ManualClock>, Duration)>,
        seen_frame: bool,
    }

    impl FakeSource {
        fn new(
            format: FrameFormat,
            frames: Vec<Result<Frame, MediaError>>,
            hang_when_empty: bool,
        ) -> (Self, Arc<AtomicBool>) {
            let stopped = Arc::new(AtomicBool::new(false));
            (
                Self {
                    format,
                    frames: frames.into(),
                    hang_when_empty,
                    stopped: stopped.clone(),
                    clock: None,
                    seen_frame: false,
                },
                stopped,
            )
        }

        /// Timestamp frame `n` at `clock + n * step`, so the first frame is
        /// due exactly when the pipeline starts and tests drive the pacer
        /// without sleeping.
        fn with_clock(mut self, clock: Arc<ManualClock>, step: Duration) -> Self {
            self.clock = Some((clock, step));
            self
        }
    }

    #[async_trait]
    impl CaptureSource for FakeSource {
        fn id(&self) -> &str {
            "fake-hls"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            if let Some(frame) = self.frames.pop_front() {
                if let Some((clock, step)) = self.clock.as_ref() {
                    if self.seen_frame {
                        clock.advance(*step);
                    }
                    self.seen_frame = true;
                }
                return frame;
            }
            if self.hang_when_empty {
                std::future::pending::<()>().await;
            }
            Err(MediaError::EndOfStream)
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    /// Manually advanced monotonic clock for deterministic pacing tests.
    struct ManualClock {
        now: Mutex<Instant>,
    }

    impl ManualClock {
        fn new(start: Instant) -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(start),
            })
        }

        fn advance(&self, step: Duration) {
            let mut now = self
                .now
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *now += step;
        }
    }

    impl HlsClock for ManualClock {
        fn now(&self) -> Instant {
            *self
                .now
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    struct RecordingSink {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        fail_write: bool,
        closed: Arc<AtomicBool>,
        closed_token: Option<Arc<AtomicBool>>,
    }

    #[async_trait]
    impl FrameSink for RecordingSink {
        async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            if self.fail_write {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "ffmpeg closed the pipe",
                ));
            }
            self.writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(bytes.to_vec());
            Ok(())
        }

        async fn close(&mut self) -> io::Result<()> {
            self.closed.store(true, Ordering::SeqCst);
            if let Some(token) = self.closed_token.as_ref() {
                token.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingSpawner {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        spawns: Arc<Mutex<Vec<(u32, u32, Vec<String>)>>>,
        closed: Arc<AtomicBool>,
        closed_sinks: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
        fail_spawn: Option<String>,
        /// 1-based spawn call that should fail, for restart-failure tests.
        fail_spawn_on: Option<usize>,
        fail_write: bool,
    }

    impl RecordingSpawner {
        fn record_spawn(
            &self,
            config: &HlsConfig,
            width: u32,
            height: u32,
            restart: bool,
        ) -> Result<SpawnedFfmpeg, HlsError> {
            let call = self
                .spawns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                + 1;
            if let Some(message) = &self.fail_spawn {
                return Err(HlsError::Spawn {
                    bin: config.ffmpeg_bin.clone(),
                    message: message.clone(),
                });
            }
            if self.fail_spawn_on == Some(call) {
                return Err(HlsError::Spawn {
                    bin: config.ffmpeg_bin.clone(),
                    message: format!("fake restart spawn failure #{call}"),
                });
            }
            self.spawns
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((
                    width,
                    height,
                    ffmpeg_argv_for(config, width, height, restart),
                ));
            let closed_token = Arc::new(AtomicBool::new(false));
            self.closed_sinks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(closed_token.clone());
            Ok(SpawnedFfmpeg {
                sink: Box::new(RecordingSink {
                    writes: self.writes.clone(),
                    fail_write: self.fail_write,
                    closed: self.closed.clone(),
                    closed_token: Some(closed_token),
                }),
                child: None,
            })
        }
    }

    impl FfmpegSpawner for RecordingSpawner {
        fn spawn(
            &self,
            config: &HlsConfig,
            width: u32,
            height: u32,
        ) -> Result<SpawnedFfmpeg, HlsError> {
            self.record_spawn(config, width, height, false)
        }

        fn spawn_generation(
            &self,
            config: &HlsConfig,
            width: u32,
            height: u32,
            restart: bool,
        ) -> Result<SpawnedFfmpeg, HlsError> {
            self.record_spawn(config, width, height, restart)
        }
    }

    fn spawner(recording: RecordingSpawner) -> Arc<dyn FfmpegSpawner> {
        Arc::new(recording)
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..400 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(condition(), "condition was not met before the timeout");
    }

    fn controller_factory(
        calls: Arc<AtomicUsize>,
        fail_on: Arc<Mutex<Vec<usize>>>,
        stopped: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
    ) -> HlsSourceFactory {
        Arc::new(move || {
            let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if fail_on
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&call)
            {
                return Err(HlsError::Source {
                    message: format!("fake source open failure #{call}"),
                });
            }
            let (source, flag) = FakeSource::new(
                FrameFormat::rgb24(2, 2),
                vec![Ok(rgb_frame(2, 2, 0, 0x55))],
                true,
            );
            stopped
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(flag);
            Ok(Box::new(source))
        })
    }

    #[test]
    fn defaults_are_the_documented_pipeline() {
        let config = HlsConfig::default();
        assert_eq!(config.segment_seconds, 1.0);
        assert_eq!(config.playlist_size, 4);
        assert_eq!(config.fps, 30);
        assert_eq!(config.bitrate_kbps, 4_000);
        assert_eq!(config.video_encoder, HlsEncoder::H264);
        assert_eq!(config.ffmpeg_bin, PathBuf::from("ffmpeg"));
        assert_eq!(config.width, None);
        assert_eq!(config.height, None);
        assert_eq!(config.gop_frames(), 30);
        assert_eq!(config.force_key_frames_expr(), "expr:gte(t,n_forced*1)");
        config.validate().expect("defaults are valid");
    }

    #[test]
    fn pacer_emits_at_most_the_configured_frames_per_second() {
        let start = Instant::now();

        // 30 frames presented at a 30 fps cadence all fit the 30 fps budget.
        let mut pacer = FramePacer::new(30, start);
        let mut emitted = 0;
        for index in 0..30 {
            let now = start + Duration::from_secs_f64(f64::from(index) / 30.0);
            if pacer.should_emit(now) {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 30, "30 fps input fits the 30 fps budget");
        assert_eq!(pacer.emitted(), 30);
        assert_eq!(pacer.dropped(), 0);

        // The same 30 frames against a 5 fps budget fit five slots in one
        // second: 0.0, 0.2, 0.4, 0.6, and 0.8 s.
        let mut pacer = FramePacer::new(5, start);
        let mut emitted = 0;
        for index in 0..30 {
            let now = start + Duration::from_secs_f64(f64::from(index) / 30.0);
            if pacer.should_emit(now) {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 5, "5 fps budget emits five of thirty frames");
        assert_eq!(pacer.emitted(), 5);
        assert_eq!(pacer.dropped(), 25);
    }

    #[test]
    fn pacer_drops_early_frames_and_admits_late_ones() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(5, start);

        // The first frame is due immediately.
        assert!(pacer.should_emit(start), "first frame is emitted at once");
        assert!(!pacer.should_emit(start + Duration::from_millis(199)));
        assert!(pacer.should_emit(start + Duration::from_millis(200)));

        // A late frame is still written.
        assert!(pacer.should_emit(start + Duration::from_millis(650)));
        // 650 ms is more than one 200 ms interval after the schedule, so the
        // next slot is 850 ms, not a banked 400 ms catch-up slot.
        assert_eq!(
            pacer.next_deadline(),
            start + Duration::from_millis(850),
            "catch-up debt is clamped to one interval"
        );
        assert!(!pacer.should_emit(start + Duration::from_millis(849)));
        assert!(pacer.should_emit(start + Duration::from_millis(850)));

        // A slightly late frame (< one interval) keeps the original phase.
        let mut pacer = FramePacer::new(10, start);
        assert!(pacer.should_emit(start));
        assert!(pacer.should_emit(start + Duration::from_millis(120)));
        assert_eq!(
            pacer.next_deadline(),
            start + Duration::from_millis(200),
            "small lateness does not rebase the schedule"
        );
        assert!(!pacer.should_emit(start + Duration::from_millis(199)));
        assert!(pacer.should_emit(start + Duration::from_millis(200)));
    }

    #[test]
    fn pacer_rebase_discards_catch_up_debt_after_a_restart() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(5, start);

        assert!(pacer.should_emit(start));
        assert_eq!(pacer.emitted(), 1);
        // Simulate an encoder restart that finished much later than the old
        // schedule. Rebase from the completion time: the next slot is one
        // interval later, not a banked burst of catch-up frames.
        let restarted_at = start + Duration::from_secs(3);
        pacer.rebase(restarted_at);
        assert_eq!(
            pacer.next_deadline(),
            restarted_at + Duration::from_millis(200)
        );
        assert!(!pacer.should_emit(restarted_at));
        assert!(!pacer.should_emit(restarted_at + Duration::from_millis(199)));
        assert!(pacer.should_emit(restarted_at + Duration::from_millis(200)));
        assert_eq!(pacer.emitted(), 2, "rebase preserves emitted counters");
        assert_eq!(pacer.dropped(), 2);
    }

    #[test]
    fn pacer_interval_is_a_positive_duration_for_every_valid_fps() {
        for fps in [1, 5, 24, 30, 60, u32::MAX] {
            assert!(
                FramePacer::interval_for(fps) >= Duration::from_nanos(1),
                "fps {fps} must have a positive interval"
            );
        }
        assert_eq!(FramePacer::interval_for(5), Duration::from_millis(200));
        assert_eq!(
            FramePacer::interval_for(30),
            Duration::from_nanos(33_333_333)
        );
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        let base = HlsConfig::default();

        for mutate in [
            |config: &mut HlsConfig| config.segment_seconds = 0.0,
            |config: &mut HlsConfig| config.segment_seconds = -1.0,
            |config: &mut HlsConfig| config.segment_seconds = f32::NAN,
            |config: &mut HlsConfig| config.segment_seconds = f32::INFINITY,
            |config: &mut HlsConfig| config.playlist_size = 0,
            |config: &mut HlsConfig| config.fps = 0,
            |config: &mut HlsConfig| config.bitrate_kbps = 0,
            |config: &mut HlsConfig| config.output_dir = PathBuf::new(),
            |config: &mut HlsConfig| config.ffmpeg_bin = PathBuf::new(),
            |config: &mut HlsConfig| config.width = Some(640),
            |config: &mut HlsConfig| {
                config.width = Some(0);
                config.height = Some(480);
            },
        ] {
            let mut config = base.clone();
            mutate(&mut config);
            assert!(
                matches!(config.validate(), Err(HlsError::InvalidConfig(_))),
                "{config:?} should be rejected"
            );
        }
    }

    #[test]
    fn ffmpeg_argv_matches_the_compatibility_pipeline() {
        let config = HlsConfig {
            output_dir: PathBuf::from("/tmp/ozma-stream-node"),
            ..HlsConfig::default()
        };
        let argv = ffmpeg_argv(&config, 640, 480);
        assert_eq!(
            argv,
            vec![
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "warning",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "-s",
                "640x480",
                "-r",
                "30",
                "-i",
                "-",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-tune",
                "zerolatency",
                "-b:v",
                "4000k",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "30",
                "-keyint_min",
                "30",
                "-sc_threshold",
                "0",
                "-force_key_frames",
                "expr:gte(t,n_forced*1)",
                "-f",
                "hls",
                "-hls_time",
                "1",
                "-hls_list_size",
                "4",
                "-hls_flags",
                "delete_segments",
                "-hls_segment_filename",
                "/tmp/ozma-stream-node/seg_%05d.ts",
                "/tmp/ozma-stream-node/stream.m3u8",
            ]
        );

        let fractional = ffmpeg_argv(
            &HlsConfig {
                segment_seconds: 0.5,
                fps: 15,
                bitrate_kbps: 1_500,
                ..config
            },
            1280,
            720,
        );
        assert!(fractional.contains(&"0.5".to_owned()));
        assert!(fractional.contains(&"15".to_owned()));
        assert!(fractional.contains(&"1500k".to_owned()));
        assert!(fractional.contains(&"1280x720".to_owned()));
    }

    #[test]
    fn gop_frames_follow_fps_and_segment_duration() {
        for (fps, segment_seconds, expected) in [
            (30, 1.0, 30),
            (5, 1.0, 5),
            (24, 2.5, 60),
            (15, 0.5, 8),
            (1, 0.4, 1), // rounds to zero, so the minimum is one frame
            (30, 0.001, 1),
            (u32::MAX, 1.0, MAX_GOP_FRAMES),
            (u32::MAX, f32::MAX, MAX_GOP_FRAMES), // saturates instead of overflowing
            (30, 1.0e30, MAX_GOP_FRAMES),
        ] {
            let config = HlsConfig {
                fps,
                segment_seconds,
                ..HlsConfig::default()
            };
            assert_eq!(
                config.gop_frames(),
                expected,
                "fps {fps} x {segment_seconds}s"
            );
            assert!(config.gop_frames() >= 1);
        }
    }

    #[test]
    fn fractional_segment_duration_forces_keyframes_on_time() {
        let config = HlsConfig {
            segment_seconds: 2.5,
            fps: 24,
            ..HlsConfig::default()
        };
        assert_eq!(config.force_key_frames_expr(), "expr:gte(t,n_forced*2.5)");

        let argv = ffmpeg_argv(&config, 640, 480);
        assert!(
            argv.windows(2).any(|window| window == ["-g", "60"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|window| window == ["-force_key_frames", "expr:gte(t,n_forced*2.5)"]),
            "{argv:?}"
        );
        assert!(
            argv.windows(2).any(|window| window == ["-hls_time", "2.5"]),
            "{argv:?}"
        );
    }

    #[test]
    fn ffmpeg_argv_selects_each_supported_encoder_and_prunes_segments() {
        let base = HlsConfig {
            output_dir: PathBuf::from("/tmp/ozma-stream-encoders"),
            ..HlsConfig::default()
        };

        for (encoder, expected_name, expected_codec_args) in [
            (
                HlsEncoder::H264,
                "libx264",
                vec!["-preset", "veryfast", "-tune", "zerolatency"],
            ),
            (
                HlsEncoder::Hevc,
                "libx265",
                vec!["-preset", "ultrafast", "-tune", "zerolatency"],
            ),
            (HlsEncoder::Av1Svt, "libsvtav1", vec!["-preset", "10"]),
            (
                HlsEncoder::Av1Aom,
                "libaom-av1",
                vec!["-cpu-used", "8", "-row-mt", "1"],
            ),
            (HlsEncoder::Mjpeg, "mjpeg", vec!["-q:v", "5"]),
        ] {
            let argv = ffmpeg_argv(
                &HlsConfig {
                    video_encoder: encoder,
                    ..base.clone()
                },
                320,
                240,
            );
            let codec_index = argv
                .iter()
                .position(|argument| argument == "-c:v")
                .unwrap_or_else(|| panic!("{encoder:?} argv has no -c:v"));
            assert_eq!(argv[codec_index + 1], expected_name, "{encoder:?}");
            for pair in expected_codec_args.chunks(2) {
                let index = argv
                    .iter()
                    .position(|argument| argument == pair[0])
                    .unwrap_or_else(|| panic!("{encoder:?} argv is missing {}", pair[0]));
                assert_eq!(argv[index + 1], pair[1], "{encoder:?}");
            }
            assert!(
                argv.windows(2)
                    .any(|window| window == ["-hls_flags", "delete_segments"]),
                "{encoder:?} argv must prune segments"
            );
            assert!(
                argv.windows(2).any(|window| window == ["-g", "30"]),
                "{encoder:?} argv must pin the GOP to the segment duration"
            );
            assert!(
                argv.windows(2)
                    .any(|window| window == ["-keyint_min", "30"]),
                "{encoder:?} argv must keep scene changes from shortening the GOP"
            );
            assert!(
                argv.windows(2)
                    .any(|window| window == ["-sc_threshold", "0"]),
                "{encoder:?} argv must disable scene-cut keyframes"
            );
            assert!(
                argv.windows(2)
                    .any(|window| window == ["-force_key_frames", "expr:gte(t,n_forced*1)"]),
                "{encoder:?} argv must force a keyframe on each segment boundary"
            );
        }
    }

    #[test]
    fn ffmpeg_argv_carries_each_hardware_encoder_upload_and_rate_control() {
        let base = HlsConfig {
            output_dir: PathBuf::from("/tmp/ozma-stream-hw"),
            vaapi_device: PathBuf::from("/dev/dri/renderD129"),
            ..HlsConfig::default()
        };

        for (encoder, expected_name, expected_pairs, expected_vf) in [
            (
                HlsEncoder::VaapiH264,
                "h264_vaapi",
                vec![("-vaapi_device", "/dev/dri/renderD129")],
                Some("format=nv12,hwupload"),
            ),
            (
                HlsEncoder::VaapiHevc,
                "hevc_vaapi",
                vec![("-vaapi_device", "/dev/dri/renderD129")],
                Some("format=nv12,hwupload"),
            ),
            (
                HlsEncoder::NvencH264,
                "h264_nvenc",
                vec![("-preset", "p4"), ("-tune", "ll"), ("-rc", "cbr")],
                None,
            ),
            (
                HlsEncoder::NvencHevc,
                "hevc_nvenc",
                vec![("-preset", "p4"), ("-tune", "ll"), ("-rc", "cbr")],
                None,
            ),
            (
                HlsEncoder::QsvH264,
                "h264_qsv",
                vec![("-preset", "veryfast")],
                None,
            ),
            (
                HlsEncoder::QsvHevc,
                "hevc_qsv",
                vec![("-preset", "veryfast")],
                None,
            ),
            (HlsEncoder::V4l2m2mH264, "h264_v4l2m2m", vec![], None),
            (HlsEncoder::V4l2m2mHevc, "hevc_v4l2m2m", vec![], None),
        ] {
            let argv = ffmpeg_argv(
                &HlsConfig {
                    video_encoder: encoder,
                    ..base.clone()
                },
                320,
                240,
            );
            let codec_index = argv
                .iter()
                .position(|argument| argument == "-c:v")
                .unwrap_or_else(|| panic!("{encoder:?} argv has no -c:v"));
            assert_eq!(argv[codec_index + 1], expected_name, "{encoder:?}");
            for (flag, value) in expected_pairs {
                let index = argv
                    .iter()
                    .position(|argument| argument == flag)
                    .unwrap_or_else(|| panic!("{encoder:?} argv is missing {flag}"));
                assert_eq!(argv[index + 1], value, "{encoder:?}");
            }
            if let Some(vf) = expected_vf {
                assert!(
                    argv.windows(2).any(|window| window == ["-vf", vf]),
                    "{encoder:?} argv must upload frames: {argv:?}"
                );
            }
            assert!(argv.contains(&"4000k".to_owned()), "{encoder:?}");
            assert!(
                argv.windows(2).any(|window| window == ["-g", "30"]),
                "{encoder:?} argv must pin the GOP to the segment duration"
            );
            assert!(
                argv.windows(2)
                    .any(|window| window == ["-force_key_frames", "expr:gte(t,n_forced*1)"]),
                "{encoder:?} argv must force a keyframe on each segment boundary"
            );
            // These are software-encoder controls; hardware wrappers do not
            // all honour them, so they must stay out of the hardware argv.
            assert!(
                !argv.iter().any(|argument| argument == "-keyint_min"),
                "{encoder:?} argv must not carry software-only -keyint_min"
            );
            assert!(
                !argv.iter().any(|argument| argument == "-sc_threshold"),
                "{encoder:?} argv must not carry software-only -sc_threshold"
            );
            assert!(
                argv.windows(2)
                    .any(|window| window == ["-hls_flags", "delete_segments"]),
                "{encoder:?} argv must prune segments"
            );
        }
    }

    #[test]
    fn ffmpeg_argv_pins_a_decodable_chroma_format() {
        let base = HlsConfig {
            output_dir: PathBuf::from("/tmp/ozma-stream-chroma"),
            ..HlsConfig::default()
        };

        for (encoder, expected) in [
            (HlsEncoder::H264, Some("yuv420p")),
            (HlsEncoder::Hevc, Some("yuv420p")),
            (HlsEncoder::Av1Svt, Some("yuv420p")),
            (HlsEncoder::Av1Aom, Some("yuv420p")),
            (HlsEncoder::Mjpeg, None),
            (HlsEncoder::VaapiH264, None),
            (HlsEncoder::VaapiHevc, None),
            (HlsEncoder::NvencH264, Some("yuv420p")),
            (HlsEncoder::NvencHevc, Some("yuv420p")),
            (HlsEncoder::QsvH264, Some("nv12")),
            (HlsEncoder::QsvHevc, Some("nv12")),
            (HlsEncoder::V4l2m2mH264, Some("yuv420p")),
            (HlsEncoder::V4l2m2mHevc, Some("yuv420p")),
        ] {
            let argv = ffmpeg_argv(
                &HlsConfig {
                    video_encoder: encoder,
                    ..base.clone()
                },
                320,
                240,
            );
            let formats: Vec<&str> = argv
                .windows(2)
                .filter(|window| window[0] == "-pix_fmt")
                .map(|window| window[1].as_str())
                .collect();
            // The first value is always the rawvideo input format.
            let expected: Vec<&str> = match expected {
                Some(format) => vec!["rgb24", format],
                None => vec!["rgb24"],
            };
            assert_eq!(formats, expected, "{encoder:?} chroma target");
        }
    }

    #[test]
    fn encoder_names_and_hw_accel_aliases_are_parsed_honestly() {
        assert_eq!(HlsEncoder::parse("libx264").unwrap(), HlsEncoder::H264);
        assert_eq!(HlsEncoder::parse("hevc").unwrap(), HlsEncoder::Hevc);
        assert_eq!(HlsEncoder::parse("libsvtav1").unwrap(), HlsEncoder::Av1Svt);
        assert_eq!(HlsEncoder::parse("libaom-av1").unwrap(), HlsEncoder::Av1Aom);
        assert_eq!(HlsEncoder::parse("mjpeg").unwrap(), HlsEncoder::Mjpeg);

        for (name, expected) in [
            ("h264_vaapi", HlsEncoder::VaapiH264),
            ("hevc_vaapi", HlsEncoder::VaapiHevc),
            ("h264_nvenc", HlsEncoder::NvencH264),
            ("hevc_nvenc", HlsEncoder::NvencHevc),
            ("h264_qsv", HlsEncoder::QsvH264),
            ("hevc_qsv", HlsEncoder::QsvHevc),
            ("h264_v4l2m2m", HlsEncoder::V4l2m2mH264),
            ("hevc_v4l2m2m", HlsEncoder::V4l2m2mHevc),
        ] {
            assert_eq!(HlsEncoder::parse(name).unwrap(), expected, "{name}");
        }

        for (alias, codec, expected) in [
            ("vaapi", "h264", HlsEncoder::VaapiH264),
            ("vaapi", "h265", HlsEncoder::VaapiHevc),
            ("nvenc", "h264", HlsEncoder::NvencH264),
            ("nvenc", "hevc", HlsEncoder::NvencHevc),
            ("qsv", "h264", HlsEncoder::QsvH264),
            ("qsv", "hevc", HlsEncoder::QsvHevc),
            ("v4l2m2m", "h264", HlsEncoder::V4l2m2mH264),
            ("v4l2m2m", "hevc", HlsEncoder::V4l2m2mHevc),
        ] {
            assert_eq!(
                HlsEncoder::parse_hw_accel(alias, codec).unwrap(),
                expected,
                "{alias}/{codec}"
            );
        }

        // A bare backend alias has no codec and is not guessed.
        for alias in ["vaapi", "nvenc", "qsv", "v4l2m2m"] {
            let error = HlsEncoder::parse(alias).expect_err("alias needs a codec");
            assert!(
                matches!(error, HlsError::UnsupportedEncoder { .. }),
                "{alias}: {error}"
            );
        }

        // Unsupported hardware codec combinations and foreign encoders are
        // typed errors, never a silent software substitution.
        for (alias, codec) in [("vaapi", "av1"), ("nvenc", "mjpeg"), ("qsv", "vp9")] {
            let error = HlsEncoder::parse_with_codec(alias, Some(codec))
                .expect_err("unsupported hardware codec");
            assert!(
                matches!(error, HlsError::UnsupportedEncoder { .. }),
                "{alias}/{codec}: {error}"
            );
        }
        for hardware in ["hevc_videotoolbox", "av1_nvenc", "vp9_vaapi"] {
            let error = HlsEncoder::parse(hardware).expect_err("unsupported hardware");
            assert!(
                matches!(error, HlsError::UnsupportedEncoder { .. }),
                "{hardware}: {error}"
            );
        }
        assert!(matches!(
            HlsEncoder::parse("vp9"),
            Err(HlsError::UnsupportedEncoder { .. })
        ));
    }

    #[tokio::test]
    async fn controller_restart_stops_the_old_source_and_applies_the_selected_encoder() {
        let directory = temp_dir("controller-restart");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_on = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let spawns = recording.spawns.clone();
        let controller = HlsController::with_spawner(
            controller_factory(calls, fail_on, stopped.clone()),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        );

        let status = controller.start().await.expect("start controller");
        assert!(matches!(
            status.state(),
            HlsState::Starting | HlsState::Running
        ));
        assert!(controller.is_running());
        wait_until(|| writes.lock().unwrap().len() == 1).await;

        let status = controller
            .restart_with_encoder(HlsEncoder::Hevc, Some(1_500))
            .await
            .expect("restart");
        assert!(matches!(
            status.state(),
            HlsState::Starting | HlsState::Running
        ));
        assert_eq!(controller.config().video_encoder, HlsEncoder::Hevc);
        assert_eq!(controller.config().bitrate_kbps, 1_500);

        let spawns = spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 2, "one spawn per generation");
        let argv = &spawns[1].2;
        assert!(argv.windows(2).any(|window| window == ["-c:v", "libx265"]));
        assert!(argv.contains(&"1500k".to_owned()));
        assert!(argv
            .windows(2)
            .any(|window| window == ["-hls_flags", "delete_segments"]));
        assert!(
            stopped.lock().unwrap()[0].load(Ordering::SeqCst),
            "the old pipeline source must stop before the restart completes"
        );

        wait_until(|| writes.lock().unwrap().len() == 2).await;
        controller.stop().await.expect("stop");
        assert_eq!(controller.status().state(), HlsState::Stopped);
        assert!(!controller.is_running());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn controller_restart_failure_restores_the_previous_encoder() {
        let directory = temp_dir("controller-restore");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_on = Arc::new(Mutex::new(vec![2]));
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let controller = HlsController::with_spawner(
            controller_factory(calls, fail_on, stopped.clone()),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        );
        controller.start().await.expect("start controller");

        let error = controller
            .restart_with_encoder(HlsEncoder::Hevc, None)
            .await
            .expect_err("new source cannot open");
        assert!(matches!(error, HlsError::Restart { .. }), "{error}");
        assert!(error.to_string().contains("restored the previous pipeline"));
        assert!(controller.is_running(), "the old encoder must be restored");
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);

        let spawns = spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 2, "initial generation plus restored one");
        assert!(spawns[1]
            .2
            .windows(2)
            .any(|window| window == ["-c:v", "libx264"]));
        assert!(stopped.lock().unwrap()[0].load(Ordering::SeqCst));
        controller.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn controller_restart_failure_without_restoration_leaves_hls_stopped() {
        let directory = temp_dir("controller-stopped");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_on = Arc::new(Mutex::new(vec![2, 3]));
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let controller = HlsController::with_spawner(
            controller_factory(calls, fail_on, stopped),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(RecordingSpawner::default()),
        );
        controller.start().await.expect("start controller");

        let error = controller
            .restart_with_encoder(HlsEncoder::Hevc, None)
            .await
            .expect_err("both generations fail");
        assert!(matches!(error, HlsError::Restart { .. }), "{error}");
        assert!(error.to_string().contains("also failed"));
        assert!(!controller.is_running());
        assert_eq!(controller.status().state(), HlsState::Failed);
        assert!(controller
            .status()
            .error()
            .unwrap_or_default()
            .contains("restoring the previous pipeline"));

        controller.stop().await.expect("stop is a no-op");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn controller_start_reports_a_typed_source_error_and_restart_requires_a_pipeline() {
        let directory = temp_dir("controller-source-error");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_on = Arc::new(Mutex::new(vec![1]));
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let controller = HlsController::with_spawner(
            controller_factory(calls, fail_on, stopped),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(RecordingSpawner::default()),
        );

        let error = controller.start().await.expect_err("no source");
        assert!(matches!(error, HlsError::Source { .. }), "{error}");
        assert_eq!(controller.status().state(), HlsState::Failed);
        assert!(!controller.is_running());

        let error = controller
            .restart_with_encoder(HlsEncoder::Hevc, None)
            .await
            .expect_err("nothing to restart");
        assert!(matches!(error, HlsError::NotRunning), "{error}");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn rgb24_frames_are_written_unchanged_and_in_order() {
        let directory = temp_dir("rgb24");
        let format = FrameFormat::rgb24(2, 2);
        let first = rgb_frame(2, 2, 0, 0x10);
        let second = rgb_frame(2, 2, 1, 0x20);
        let (source, stopped) = FakeSource::new(format, vec![Ok(first), Ok(second)], false);
        let clock = ManualClock::new(Instant::now());
        let source = source.with_clock(clock.clone(), Duration::from_millis(50));

        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let mut pipeline = HlsPipeline::start_with_clock(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::software()),
            clock,
        )
        .expect("start pipeline");

        wait_until(|| writes.lock().unwrap().len() == 2).await;
        wait_until(|| stopped.load(Ordering::SeqCst)).await;

        let writes = writes.lock().unwrap().clone();
        assert_eq!(writes[0], vec![0x10; 2 * 2 * 3]);
        assert_eq!(writes[1], vec![0x20; 2 * 2 * 3]);
        assert!(!pipeline.is_running());
        assert_eq!(pipeline.status().state(), HlsState::Stopped);
        pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn pipeline_pacer_forwards_at_most_the_configured_rate_and_stops_on_eos() {
        let directory = temp_dir("pacer-rate");
        let format = FrameFormat::rgb24(1, 1);
        let frames = (0..10)
            .map(|sequence| Ok(rgb_frame(1, 1, sequence, sequence as u8)))
            .collect();
        let clock = ManualClock::new(Instant::now());
        let (source, stopped) = FakeSource::new(format, frames, false);
        // Ten frames arrive every 100 ms (10 fps of input) against a 2 fps
        // HLS timebase. Slots are 500 ms, so only two frames are written.
        let source = source.with_clock(clock.clone(), Duration::from_millis(100));

        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let closed = recording.closed.clone();
        let mut pipeline = HlsPipeline::start_with_clock(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                fps: 2,
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::software()),
            clock,
        )
        .expect("start pipeline");

        wait_until(|| stopped.load(Ordering::SeqCst) && writes.lock().unwrap().len() == 2).await;
        // EOS is clean: the sink is closed, the source stops, and the status
        // is `stopped`, not `failed`.
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(pipeline.status().state(), HlsState::Stopped);
        assert!(!pipeline.is_running());
        assert_eq!(writes.lock().unwrap().len(), 2);
        pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn bgra_frames_are_converted_to_rgb24() {
        let directory = temp_dir("bgra");
        let format = FrameFormat::new(PixelFormat::Bgra, 2, 1);
        let bgra = vec![
            1, 2, 3, 255, // B G R A -> R G B
            4, 5, 6, 128, // B G R A -> R G B
        ];
        let (source, _) = FakeSource::new(format, vec![Ok(frame(format, 0, bgra))], false);
        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let mut pipeline = HlsPipeline::start_with_spawner(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        )
        .expect("start pipeline");

        wait_until(|| writes.lock().unwrap().len() == 1).await;
        assert_eq!(writes.lock().unwrap()[0], vec![3, 2, 1, 6, 5, 4]);
        pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn unsupported_pixel_formats_are_rejected_before_spawning() {
        let directory = temp_dir("yuyv");
        let format = FrameFormat::new(PixelFormat::Yuyv, 2, 2);
        let (source, _) = FakeSource::new(format, vec![], false);
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();

        let error = HlsPipeline::start_with_spawner(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        )
        .err()
        .expect("yuyv must be rejected");

        assert!(matches!(
            error,
            HlsError::UnsupportedFormat(PixelFormat::Yuyv)
        ));
        assert!(
            spawns.lock().unwrap().is_empty(),
            "ffmpeg must not be spawned for an unsupported format"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn write_failure_stops_the_pipeline_and_is_reported() {
        let directory = temp_dir("write-failure");
        let format = FrameFormat::rgb24(1, 1);
        let (source, stopped) = FakeSource::new(format, vec![Ok(rgb_frame(1, 1, 0, 0x7f))], false);
        let recording = RecordingSpawner {
            fail_write: true,
            ..RecordingSpawner::default()
        };
        let mut pipeline = HlsPipeline::start_with_spawner(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        )
        .expect("start pipeline");

        wait_until(|| pipeline.status().state() == HlsState::Failed).await;
        wait_until(|| stopped.load(Ordering::SeqCst)).await;
        let status = pipeline.status();
        assert_eq!(status.status_name(), "failed");
        assert!(status
            .error()
            .unwrap_or_default()
            .contains("writing a frame to ffmpeg failed"));
        assert!(!pipeline.is_running());
        pipeline.stop().await.expect("stop");
        assert_eq!(pipeline.status().state(), HlsState::Failed);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_ends_the_frame_task() {
        let directory = temp_dir("stop");
        let format = FrameFormat::rgb24(1, 1);
        let (source, stopped) = FakeSource::new(format, vec![Ok(rgb_frame(1, 1, 0, 0xaa))], true);
        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let closed = recording.closed.clone();
        let mut pipeline = HlsPipeline::start_with_spawner(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        )
        .expect("start pipeline");

        wait_until(|| writes.lock().unwrap().len() == 1).await;
        assert!(pipeline.is_running());

        pipeline.stop().await.expect("first stop");
        pipeline.stop().await.expect("second stop is a no-op");

        assert!(!pipeline.is_running());
        assert!(stopped.load(Ordering::SeqCst));
        assert!(closed.load(Ordering::SeqCst));
        assert_eq!(writes.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn unknown_dimensions_are_learned_from_the_first_frame() {
        let directory = temp_dir("lazy");
        let format = FrameFormat::rgb24(0, 0);
        let frame = Frame::with_data(FrameFormat::rgb24(4, 2), 0, 0, true, vec![0x33; 4 * 2 * 3])
            .expect("valid frame");
        let (source, _) = FakeSource::new(format, vec![Ok(frame)], false);
        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let spawns = recording.spawns.clone();

        let mut pipeline = HlsPipeline::start_with_spawner(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        )
        .expect("start pipeline");

        wait_until(|| writes.lock().unwrap().len() == 1).await;
        let spawns = spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0].0, 4);
        assert_eq!(spawns[0].1, 2);
        assert!(spawns[0].2.contains(&"4x2".to_owned()));
        pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn frame_size_change_restarts_ffmpeg_at_the_new_size_and_appends_the_playlist() {
        let directory = temp_dir("resolution-change");
        let format = FrameFormat::rgb24(0, 0);
        let first = rgb_frame(640, 480, 0, 0x11);
        let second = rgb_frame(1280, 800, 1, 0x22);
        let clock = ManualClock::new(Instant::now());
        let (source, stopped) = FakeSource::new(format, vec![Ok(first), Ok(second)], false);
        let source = source.with_clock(clock.clone(), Duration::from_millis(200));

        let recording = RecordingSpawner::default();
        let writes = recording.writes.clone();
        let spawns = recording.spawns.clone();
        let closed_sinks = recording.closed_sinks.clone();
        let mut pipeline = HlsPipeline::start_with_clock(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                fps: 5,
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::software()),
            clock,
        )
        .expect("start pipeline");

        wait_until(|| writes.lock().unwrap().len() == 2 && stopped.load(Ordering::SeqCst)).await;
        let spawns = spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 2, "a size change spawns one new generation");
        assert_eq!((spawns[0].0, spawns[0].1), (640, 480));
        assert_eq!((spawns[1].0, spawns[1].1), (1280, 800));
        assert!(spawns[0].2.contains(&"640x480".to_owned()));
        assert!(spawns[1].2.contains(&"1280x800".to_owned()));
        assert!(
            spawns[0].2.contains(&"delete_segments".to_owned())
                && !spawns[0]
                    .2
                    .contains(&"delete_segments+append_list+discont_start".to_owned()),
            "the first generation starts a fresh playlist"
        );
        assert!(
            spawns[1]
                .2
                .contains(&"delete_segments+append_list+discont_start".to_owned()),
            "a restart appends to the playlist and marks a discontinuity"
        );
        for expected in ["libx264", "4000k", "-hls_time", "1", "-hls_list_size", "4"] {
            assert!(
                spawns[1].2.contains(&expected.to_owned()),
                "restart generation keeps `{expected}`: {:?}",
                spawns[1].2
            );
        }
        assert_eq!(
            spawns[1].2.last().map(|argument| argument.as_str()),
            Some(directory.join("stream.m3u8").to_str().unwrap()),
            "a restart writes the same playlist path"
        );

        let sinks = closed_sinks.lock().unwrap();
        assert_eq!(sinks.len(), 2);
        // The fake spawner has no OS child, so the observable stop signal is
        // its sink close; `run_pipeline` calls `kill_child` on the real child
        // immediately after closing that sink.
        assert!(
            sinks[0].load(Ordering::SeqCst),
            "the old ffmpeg generation's stdin/child must be stopped first"
        );

        let writes = writes.lock().unwrap();
        assert_eq!(writes[0].len(), 640 * 480 * 3);
        assert_eq!(writes[1].len(), 1280 * 800 * 3);
        assert_eq!(writes[0][0], 0x11);
        assert_eq!(writes[1][0], 0x22);
        drop(writes);

        assert_eq!(pipeline.status().state(), HlsState::Stopped);
        pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn pinned_dimensions_reject_a_frame_size_change_instead_of_scaling() {
        let directory = temp_dir("pinned-resolution");
        let format = FrameFormat::rgb24(640, 480);
        let clock = ManualClock::new(Instant::now());
        let (source, stopped) = FakeSource::new(
            format,
            vec![
                Ok(rgb_frame(640, 480, 0, 0x33)),
                Ok(rgb_frame(1280, 800, 1, 0x44)),
            ],
            false,
        );
        let source = source.with_clock(clock.clone(), Duration::from_millis(200));

        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let writes = recording.writes.clone();
        let mut pipeline = HlsPipeline::start_with_clock(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                fps: 5,
                width: Some(640),
                height: Some(480),
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::software()),
            clock,
        )
        .expect("start pipeline");

        wait_until(|| {
            stopped.load(Ordering::SeqCst) && pipeline.status().state() == HlsState::Failed
        })
        .await;
        assert_eq!(
            spawns.lock().unwrap().len(),
            1,
            "pinned dimensions never restart"
        );
        assert_eq!(
            writes.lock().unwrap().len(),
            1,
            "the bad frame is not written"
        );
        let error = pipeline.status().error().unwrap_or_default();
        assert!(
            error.contains("frame dimensions 1280x800 do not match the HLS input 640x480"),
            "unexpected error: {error}"
        );
        assert!(!pipeline.is_running());
        pipeline.stop().await.expect("stop");
        assert_eq!(pipeline.status().state(), HlsState::Failed);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn failed_restart_marks_the_pipeline_failed_and_does_not_feed_the_dead_child() {
        let directory = temp_dir("resolution-restart-failure");
        let format = FrameFormat::rgb24(640, 480);
        let clock = ManualClock::new(Instant::now());
        let (source, stopped) = FakeSource::new(
            format,
            vec![
                Ok(rgb_frame(640, 480, 0, 0x55)),
                Ok(rgb_frame(1280, 800, 1, 0x66)),
            ],
            false,
        );
        let source = source.with_clock(clock.clone(), Duration::from_millis(200));

        let recording = RecordingSpawner {
            fail_spawn_on: Some(2),
            ..RecordingSpawner::default()
        };
        let spawns = recording.spawns.clone();
        let writes = recording.writes.clone();
        let closed_sinks = recording.closed_sinks.clone();
        let mut pipeline = HlsPipeline::start_with_clock(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                fps: 5,
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::software()),
            clock,
        )
        .expect("start pipeline");

        wait_until(|| {
            stopped.load(Ordering::SeqCst) && pipeline.status().state() == HlsState::Failed
        })
        .await;
        assert_eq!(spawns.lock().unwrap().len(), 1);
        assert_eq!(
            writes.lock().unwrap().len(),
            1,
            "the new-size frame is never written to the dead child"
        );
        let error = pipeline.status().error().unwrap_or_default();
        assert!(
            error.contains("HLS restart failed"),
            "unexpected error: {error}"
        );
        assert!(error.contains("1280x800"), "unexpected error: {error}");
        assert!(
            closed_sinks.lock().unwrap()[0].load(Ordering::SeqCst),
            "the old child is stopped even when the restart fails"
        );
        assert!(!pipeline.is_running());
        pipeline.stop().await.expect("stop");
        assert_eq!(pipeline.status().state(), HlsState::Failed);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn hls_status_never_claims_running_before_the_manifest_exists() {
        let directory = temp_dir("status");
        let manifest = directory.join("stream.m3u8");
        let status = HlsStatus::starting(manifest.clone());

        assert_eq!(status.state(), HlsState::Starting);
        assert_eq!(status.status_name(), "failed");
        assert!(status
            .error()
            .unwrap_or_default()
            .contains("has not been written yet"));

        std::fs::write(&manifest, "#EXTM3U\n").expect("write manifest");
        assert_eq!(status.status_name(), "running");
        assert_eq!(status.error(), None);

        status.set_failed("ffmpeg exited");
        assert_eq!(status.status_name(), "failed");
        assert_eq!(status.error().as_deref(), Some("ffmpeg exited"));

        assert_eq!(HlsStatus::not_configured().status_name(), "not-configured");
        assert_eq!(
            HlsStatus::failed("no capture source").status_name(),
            "failed"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn spawn_failure_is_a_typed_error() {
        let directory = temp_dir("spawn-failure");
        let (source, _) = FakeSource::new(
            FrameFormat::rgb24(2, 2),
            vec![Ok(rgb_frame(2, 2, 0, 0))],
            false,
        );
        let error = HlsPipeline::start(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                ffmpeg_bin: PathBuf::from("/nonexistent/ozma-test-ffmpeg"),
                ..HlsConfig::default()
            },
        )
        .err()
        .expect("spawn must fail");
        assert!(matches!(error, HlsError::Spawn { .. }));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn hardware_start_reports_missing_vaapi_device_and_unavailable_encoder() {
        let directory = temp_dir("hardware-availability");
        let missing_device = directory.join("missing-renderD128");

        let hardware_probe = Arc::new(StaticEncoderProbe::with_encoders([
            HlsEncoder::H264,
            HlsEncoder::VaapiH264,
        ]));
        let (source, _) = FakeSource::new(
            FrameFormat::rgb24(2, 2),
            vec![Ok(rgb_frame(2, 2, 0, 0))],
            false,
        );
        let error = HlsPipeline::start_with_spawner_and_probe(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                video_encoder: HlsEncoder::VaapiH264,
                vaapi_device: missing_device.clone(),
                ..HlsConfig::default()
            },
            spawner(RecordingSpawner::default()),
            hardware_probe.clone(),
        )
        .err()
        .expect("missing VAAPI device must fail");
        assert!(matches!(error, HlsError::VaapiDevice { .. }), "{error}");
        assert!(error.to_string().contains("missing-renderD128"));

        // With a device path present but no matching ffmpeg encoder, the probe
        // evidence decides: no fallback, no spawn.
        let device = directory.join("renderD128");
        std::fs::write(&device, b"").expect("fake device file");
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let (source, _) = FakeSource::new(
            FrameFormat::rgb24(2, 2),
            vec![Ok(rgb_frame(2, 2, 0, 0))],
            false,
        );
        let error = HlsPipeline::start_with_spawner_and_probe(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                video_encoder: HlsEncoder::VaapiH264,
                vaapi_device: device.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::software()),
        )
        .err()
        .expect("unproven hardware must fail");
        assert!(
            matches!(error, HlsError::EncoderUnavailable { .. }),
            "{error}"
        );
        assert!(spawns.lock().unwrap().is_empty(), "no ffmpeg spawn");
        let _ = std::fs::remove_dir_all(directory);
    }

    struct RejectingRuntimeProbe;

    impl EncoderProbe for RejectingRuntimeProbe {
        fn availability(&self, _ffmpeg_bin: &Path) -> Result<EncoderAvailability, String> {
            Ok(EncoderAvailability::from_names([
                HlsEncoder::H264.ffmpeg_name(),
                HlsEncoder::VaapiH264.ffmpeg_name(),
            ]))
        }

        fn runtime_check(&self, _config: &HlsConfig) -> Result<(), String> {
            Err("the fake device rejected the probe encode".to_owned())
        }
    }

    #[tokio::test]
    async fn hardware_start_rejects_a_failed_runtime_probe_before_spawning() {
        let directory = temp_dir("hardware-runtime-probe");
        let device = directory.join("renderD128");
        std::fs::write(&device, b"").expect("fake device file");
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let (source, _) = FakeSource::new(
            FrameFormat::rgb24(2, 2),
            vec![Ok(rgb_frame(2, 2, 0, 0))],
            false,
        );

        let error = HlsPipeline::start_with_spawner_and_probe(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                video_encoder: HlsEncoder::VaapiH264,
                vaapi_device: device,
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(RejectingRuntimeProbe),
        )
        .err()
        .expect("a failed runtime probe must fail the start");

        assert!(
            matches!(error, HlsError::EncoderUnavailable { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("real encode probe"),
            "the error must name the probe: {error}"
        );
        assert!(
            spawns.lock().unwrap().is_empty(),
            "no ffmpeg pipeline may start after a failed probe"
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn hardware_start_uses_vaapi_argv_when_probe_confirms_the_encoder() {
        let directory = temp_dir("hardware-start");
        let device = directory.join("renderD128");
        std::fs::write(&device, b"").expect("fake device file");
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let (source, _) = FakeSource::new(
            FrameFormat::rgb24(2, 2),
            vec![Ok(rgb_frame(2, 2, 0, 0x33))],
            false,
        );
        let mut pipeline = HlsPipeline::start_with_spawner_and_probe(
            Box::new(source),
            HlsConfig {
                output_dir: directory.clone(),
                video_encoder: HlsEncoder::VaapiH264,
                vaapi_device: device.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::with_encoders([
                HlsEncoder::H264,
                HlsEncoder::VaapiH264,
            ])),
        )
        .expect("proven hardware starts");

        let spawns = spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 1);
        let argv = &spawns[0].2;
        assert!(argv
            .windows(2)
            .any(|window| window == ["-c:v", "h264_vaapi"]));
        assert!(argv.windows(2).any(
            |window| window[0] == "-vaapi_device" && window[1] == device.display().to_string()
        ));
        assert!(argv
            .windows(2)
            .any(|window| window == ["-vf", "format=nv12,hwupload"]));
        pipeline.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn hardware_restart_never_falls_back_without_explicit_permission() {
        let directory = temp_dir("hardware-no-fallback");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_on = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let controller = HlsController::with_spawner(
            controller_factory(calls, fail_on, stopped),
            HlsConfig {
                output_dir: directory.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
        );
        controller.start().await.expect("start controller");

        let error = controller
            .restart_with_encoder_fallback(HlsEncoder::NvencH264, None, false)
            .await
            .expect_err("unproven NVENC must not be assumed");
        assert!(
            matches!(error, HlsError::EncoderUnavailable { .. }),
            "{error}"
        );
        assert!(controller.is_running(), "previous pipeline restored");
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);
        assert_eq!(
            spawns.lock().unwrap().len(),
            2,
            "initial generation plus the restoration"
        );

        let outcome = controller
            .restart_with_encoder_fallback(HlsEncoder::NvencH264, None, true)
            .await
            .expect("explicit fallback is allowed");
        assert_eq!(outcome.requested_encoder, HlsEncoder::NvencH264);
        assert_eq!(outcome.applied_encoder, HlsEncoder::H264);
        assert!(outcome.fallback.is_some());
        assert_eq!(controller.config().video_encoder, HlsEncoder::H264);
        controller.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn controller_hardware_restart_applies_the_verified_encoder() {
        let directory = temp_dir("hardware-restart");
        let device = directory.join("renderD128");
        std::fs::write(&device, b"").expect("fake device file");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_on = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let recording = RecordingSpawner::default();
        let spawns = recording.spawns.clone();
        let controller = HlsController::with_spawner_and_probe(
            controller_factory(calls, fail_on, stopped),
            HlsConfig {
                output_dir: directory.clone(),
                vaapi_device: device.clone(),
                ..HlsConfig::default()
            },
            spawner(recording),
            Arc::new(StaticEncoderProbe::with_encoders([
                HlsEncoder::H264,
                HlsEncoder::VaapiH264,
            ])),
        );
        controller.start().await.expect("start controller");

        let outcome = controller
            .restart_with_encoder_fallback(HlsEncoder::VaapiH264, Some(3_000), false)
            .await
            .expect("verified hardware restarts");
        assert_eq!(outcome.requested_encoder, HlsEncoder::VaapiH264);
        assert_eq!(outcome.applied_encoder, HlsEncoder::VaapiH264);
        assert!(outcome.fallback.is_none());
        assert_eq!(controller.config().video_encoder, HlsEncoder::VaapiH264);
        assert_eq!(controller.config().bitrate_kbps, 3_000);

        let spawns = spawns.lock().unwrap().clone();
        assert_eq!(spawns.len(), 2);
        let argv = &spawns[1].2;
        assert!(argv
            .windows(2)
            .any(|window| window == ["-c:v", "h264_vaapi"]));
        assert!(argv.contains(&"3000k".to_owned()));
        controller.stop().await.expect("stop");
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Opt-in real-ffmpeg smoke test. Default CI must not need ffmpeg.
    #[tokio::test]
    #[ignore = "requires ffmpeg; set OZMA_FFMPEG_TEST=1 and run with --ignored"]
    async fn real_ffmpeg_writes_a_playlist() {
        if std::env::var("OZMA_FFMPEG_TEST").as_deref() != Ok("1") {
            return;
        }
        let directory = temp_dir("real-ffmpeg");
        let format = FrameFormat::rgb24(64, 48);
        let frames: Vec<_> = (0..40)
            .map(|sequence| Ok(rgb_frame(64, 48, sequence, sequence as u8)))
            .collect();
        let (source, _) = FakeSource::new(format, frames, false);
        let clock = ManualClock::new(Instant::now());
        let source = source.with_clock(clock.clone(), Duration::from_millis(100));
        let config = HlsConfig {
            output_dir: directory.clone(),
            segment_seconds: 0.5,
            playlist_size: 2,
            fps: 10,
            bitrate_kbps: 500,
            ..HlsConfig::default()
        };
        let mut pipeline = HlsPipeline::start_with_clock(
            Box::new(source),
            config,
            Arc::new(SystemFfmpegSpawner),
            Arc::new(StaticEncoderProbe::software()),
            clock,
        )
        .expect("start ffmpeg");

        for _ in 0..200 {
            if pipeline.manifest_path().is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            pipeline.manifest_path().is_file(),
            "ffmpeg did not write {}",
            pipeline.manifest_path().display()
        );
        let manifest = std::fs::read_to_string(pipeline.manifest_path()).unwrap();
        assert!(manifest.contains("#EXTM3U"));
        assert!(
            std::fs::read_dir(pipeline.output_dir())
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.path().extension().is_some_and(|ext| ext == "ts")),
            "ffmpeg did not write a segment"
        );

        pipeline.stop().await.expect("stop");
        assert!(pipeline.manifest_path().is_file());
        let _ = std::fs::remove_dir_all(directory);
    }
}
