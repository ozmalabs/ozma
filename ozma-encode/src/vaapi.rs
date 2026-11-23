// SPDX-License-Identifier: AGPL-3.0-only
//! Real VAAPI H.264 encoding through a supervised FFmpeg sidecar.
//!
//! # Why a process and not FFI
//!
//! VAAPI encoding through libavcodec needs a `AVBufferRef` hardware frames
//! context, a `AVHWDeviceContext`, and a surface pool. Neither `ffmpeg-next`
//! nor any other maintained safe binding exposes that path without raw
//! pointers, and this crate is `#![forbid(unsafe_code)]`. Native FFI may live
//! in dependencies, but a dependency that hid the surface allocation behind
//! guessed layouts would be harder to audit than the small, explicit process
//! boundary used here.
//!
//! The backend therefore runs one `ffmpeg` child process per encoder:
//!
//! ```text
//! RGB24/BGRA/NV12/YUYV/MJPEG
//!   -> convert_to_i420() (shared with the software backend)
//!   -> `-f rawvideo -pix_fmt yuv420p` on ffmpeg stdin
//!   -> `format=nv12,hwupload` onto a VAAPI surface
//!   -> `h264_vaapi` (Intel iHD, AMD, or any libva driver)
//!   -> `h264_metadata=aud=insert` (one AUD per access unit)
//!   -> FLV container on ffmpeg stdout
//!   -> per-tag access-unit extraction -> Annex-B H.264
//! ```
//!
//! # Deterministic access-unit framing
//!
//! Raw `-f h264` on a pipe has no packet boundaries, and the MPEG-TS muxer
//! writes PES packets with `PES_packet_length = 0` for video, so neither can
//! delimit one access unit without waiting for the *next* frame. This backend
//! uses the FLV muxer instead: every encoded access unit is one tag with an
//! explicit 24-bit length, and the first video tag carries the
//! `AVCDecoderConfigurationRecord` (SPS/PPS and NAL length size). Each tag is
//! parsed, its AVCC length-prefixed NAL units are converted to Annex-B, and a
//! tag is rejected unless it contains a VCL slice. This is a real container
//! parse, not a "read whatever the pipe has" heuristic.
//!
//! The `h264_metadata=aud=insert` bitstream filter runs before the muxer, so
//! each access unit also begins with an Access Unit Delimiter, which
//! downstream RTP packetisers use to find boundaries cheaply.
//!
//! # Keyframes
//!
//! Periodic keyframes come from `-g <frames>`; the encoder emits SPS, PPS, and
//! an IDR at each boundary. Keyframes are detected from the NAL units
//! themselves (IDR = type 5), not from the FLV flag, so the result is
//! authoritative even if the muxer's hint is wrong.
//!
//! A forced keyframe cannot be signalled to the `ffmpeg` command line while it
//! is running: `h264_vaapi` has no runtime keyframe request and the CLI has no
//! control channel. [`VaapiH264Encoder::request_keyframe`] therefore marks the
//! next frame and the encoder restarts its sidecar before that frame. A fresh
//! `h264_vaapi` process always begins with an IDR, so the next access unit is
//! a real keyframe. This costs one encoder re-initialisation (tens of
//! milliseconds) and is the honest trade for a safe, non-FFI integration.
//!
//! # Timing and limits
//!
//! * `-async_depth 1` is required for the trait contract: one input frame
//!   yields exactly one access unit before the next call. Larger async depths
//!   buffer frames inside FFmpeg.
//! * One process per encoder; the child is killed on drop so a crash cannot
//!   leave an orphan. [`VaapiH264Encoder::stop`] is the clean, awaited path.
//! * SDR 8-bit 4:2:0 only. P010/HDR is rejected.
//! * Frames must be tightly packed; the shared converter rejects truncated
//!   payloads.
//! * The advertised envelope is 1920x1080 at 60 fps. Larger frames may work on
//!   some devices, but the probe only proves a small frame; a node that needs
//!   4K should verify it on that device before advertising it.
//! * `ffmpeg` (with `h264_vaapi` and the `h264_metadata` bitstream filter) and
//!   a libva driver such as Intel iHD must be installed. `OZMA_FFMPEG` and
//!   `OZMA_VAAPI_DEVICE` override the binary and device path.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use ozma_media::{EncodedFrame, Encoder as _, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{
    codes, ApiError, Codec, ColorFormat, EncodeCapability, EncoderBackend, MediaFormat,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command as ProcessCommand};
use tokio::task::JoinHandle;

use crate::probe::{EncoderProbe, ProbeResult};
use crate::software::{
    annex_b_is_keyframe, annex_b_nal_types, coded_dimensions, convert_to_i420, encode_failed,
    ColorMatrix,
};

/// Default render node used when the environment does not override it.
pub const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Largest width the VAAPI backend advertises.
pub const VAAPI_MAX_WIDTH: u32 = 1920;
/// Largest height the VAAPI backend advertises.
pub const VAAPI_MAX_HEIGHT: u32 = 1080;
/// Highest frame rate the VAAPI backend advertises.
pub const VAAPI_MAX_FPS: u32 = 60;

/// Environment variable that overrides the render node.
pub const VAAPI_DEVICE_ENV: &str = "OZMA_VAAPI_DEVICE";
/// Environment variable that overrides the `ffmpeg` binary.
pub const FFMPEG_ENV: &str = "OZMA_FFMPEG";

/// Maximum time one frame may take to come back from the child process.
const FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time a clean stop waits for FFmpeg before killing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound on a single FLV tag, to bound memory on a corrupt stream.
const MAX_TAG_BYTES: usize = 32 * 1024 * 1024;
/// Upper bound on tags skipped while looking for the next video access unit.
const MAX_TAGS_PER_FRAME: usize = 64;
/// Tail of child stderr kept for diagnostics.
const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// Resolve the `ffmpeg` binary, honouring [`FFMPEG_ENV`].
pub fn ffmpeg_binary() -> OsString {
    std::env::var_os(FFMPEG_ENV).unwrap_or_else(|| OsString::from("ffmpeg"))
}

/// Whether `binary` can be executed and reports success for `-version`.
///
/// This deliberately uses `std::process` so construction does not require a
/// Tokio runtime; the long-running sidecar is started lazily by the first
/// [`ozma_media::Encoder::encode`] call, inside the caller's runtime.
fn ffmpeg_is_runnable(binary: &OsStr) -> bool {
    std::process::Command::new(binary)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Find a usable DRM render node.
///
/// The device is never guessed from a card node: VAAPI needs a render node
/// (`/dev/dri/renderD*`) so encoding does not require DRM master. The explicit
/// [`VAAPI_DEVICE_ENV`], then `/dev/dri/renderD128`, then the lowest-numbered
/// render node are tried. `None` means this host exposes no render node.
pub fn detect_vaapi_device() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(VAAPI_DEVICE_ENV) {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }

    let preferred = PathBuf::from(DEFAULT_VAAPI_DEVICE);
    if preferred.exists() {
        return Some(preferred);
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
        })
        .collect();
    candidates.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("renderD"))
            .and_then(|number| number.parse::<u32>().ok())
            .unwrap_or(u32::MAX)
    });
    candidates.into_iter().next()
}

/// A real VAAPI H.264 encoder.
///
/// Construct with [`VaapiH264Encoder::new`] (auto-detected device) or
/// [`VaapiH264Encoder::new_on`] (explicit device, used by probes).
pub struct VaapiH264Encoder {
    id: String,
    ffmpeg: OsString,
    device: PathBuf,
    input: FrameFormat,
    output: MediaFormat,
    coded_width: usize,
    coded_height: usize,
    fps: u32,
    bitrate_kbps: u32,
    keyframe_interval: u32,
    color: ColorMatrix,
    force_keyframe: bool,
    restart_needed: bool,
    last_pts: Option<u64>,
    last_sequence: Option<u64>,
    encoded: u64,
    stopped: bool,
    sidecar: Option<Sidecar>,
}

impl fmt::Debug for VaapiH264Encoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaapiH264Encoder")
            .field("id", &self.id)
            .field("device", &self.device)
            .field("input", &self.input)
            .field("output", &self.output)
            .field("keyframe_interval", &self.keyframe_interval)
            .field("encoded", &self.encoded)
            .field("running", &self.sidecar.is_some())
            .finish()
    }
}

impl VaapiH264Encoder {
    /// Build a VAAPI H.264 encoder on the auto-detected render node.
    ///
    /// # Errors
    ///
    /// Returns a structured [`ApiError`] when the codec is not H.264, HDR is
    /// requested, the input layout cannot be converted, the dimensions are
    /// unusable, no DRM render node exists, or `ffmpeg` is unavailable.
    ///
    /// The long-running sidecar is started by the first `encode` call, so this
    /// constructor can be used from synchronous code. The probe performs a
    /// real encode, which is what proves the device works.
    pub fn new(input: FrameFormat, request: MediaFormat) -> Result<Self, ApiError> {
        let device = detect_vaapi_device().ok_or_else(no_device_error)?;
        Self::new_on(device, input, request)
    }

    /// Build a VAAPI H.264 encoder on an explicit render node.
    ///
    /// # Errors
    ///
    /// As [`VaapiH264Encoder::new`], plus the device path must exist.
    pub fn new_on(
        device: impl Into<PathBuf>,
        input: FrameFormat,
        request: MediaFormat,
    ) -> Result<Self, ApiError> {
        let device = device.into();
        validate_request(&input, &request)?;
        if !device.exists() {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!("The VAAPI device `{}` does not exist.", device.display()),
            )
            .with_details("vaapi-device".to_owned())
            .with_remediation(
                "Expose a DRM render node (usually /dev/dri/renderD128) or set OZMA_VAAPI_DEVICE.",
            ));
        }

        // Fail construction early with an actionable error, but without
        // starting the long-running sidecar: that needs a Tokio runtime, while
        // `EncoderSelection::build_encoder` is a synchronous API.
        let ffmpeg = ffmpeg_binary();
        if !ffmpeg_is_runnable(&ffmpeg) {
            return Err(ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "`{}` is not a runnable ffmpeg binary, so VAAPI encoding is unavailable.",
                    ffmpeg.to_string_lossy()
                ),
            )
            .with_details("ffmpeg-spawn".to_owned())
            .with_remediation(
                "Install ffmpeg with h264_vaapi and the h264_metadata bitstream filter, or set OZMA_FFMPEG to its path.",
            ));
        }

        let (coded_width, coded_height) = coded_dimensions(input)?;
        let fps = if request.fps == 0 { 30 } else { request.fps };
        let bitrate_kbps = if request.bitrate_kbps == 0 {
            2_000
        } else {
            request.bitrate_kbps
        };
        let keyframe_interval = if request.fps == 0 {
            30
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

        let encoder = Self {
            id: "vaapi-h264".to_owned(),
            ffmpeg,
            device,
            input,
            output,
            coded_width,
            coded_height,
            fps,
            bitrate_kbps,
            keyframe_interval,
            color: ColorMatrix::for_frame(coded_width, coded_height),
            force_keyframe: false,
            restart_needed: false,
            last_pts: None,
            last_sequence: None,
            encoded: 0,
            stopped: false,
            sidecar: None,
        };
        // The process starts on the first encode, so constructing an encoder
        // works outside an async runtime and a registered-but-unused encoder
        // does not hold a process open.
        Ok(encoder)
    }

    /// The capability this backend can truthfully advertise.
    pub fn capability() -> EncodeCapability {
        EncodeCapability {
            codec: Codec::H264,
            backend: EncoderBackend::Vaapi,
            hardware: true,
            max_width: VAAPI_MAX_WIDTH,
            max_height: VAAPI_MAX_HEIGHT,
            max_fps: VAAPI_MAX_FPS,
            hdr: false,
            profiles: vec!["high".to_owned()],
        }
    }

    /// The render node this encoder uses.
    pub fn device(&self) -> &Path {
        &self.device
    }

    /// Set the periodic keyframe interval in frames.
    ///
    /// `0` disables periodic keyframes; the first frame is still an IDR and
    /// [`ozma_media::Encoder::request_keyframe`] still works. Changing this
    /// after encoding has started restarts the sidecar before the next frame
    /// so the new GOP size takes effect.
    pub fn with_keyframe_interval(mut self, frames: u32) -> Self {
        if frames != self.keyframe_interval {
            self.keyframe_interval = frames;
            // Before the first frame there is no process to restart; the new
            // interval is simply used when the sidecar starts lazily.
            if self.encoded > 0 {
                self.restart_needed = true;
            }
        }
        self
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

    /// Stop the child process cleanly and wait for it to exit.
    ///
    /// Closing stdin makes FFmpeg flush and exit; if it does not within two
    /// seconds it is killed. The encoder is unusable afterwards.
    pub async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped = true;
        let Some(mut sidecar) = self.sidecar.take() else {
            return Ok(());
        };
        sidecar.stderr_task.abort();
        drop(sidecar.stdin);
        match tokio::time::timeout(STOP_TIMEOUT, sidecar.child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(self.failure(
                format!("Failed to reap the VAAPI encoder process: {error}"),
                "ffmpeg-process",
                "Check that the ffmpeg process was not killed by the system.",
            )),
            Err(_) => {
                let _ = sidecar.child.start_kill();
                let _ = sidecar.child.wait().await;
                Ok(())
            }
        }
    }

    fn start_sidecar(&mut self) -> Result<(), ApiError> {
        if let Some(mut old) = self.sidecar.take() {
            old.stderr_task.abort();
            let _ = old.child.start_kill();
        }

        let gop = if self.keyframe_interval == 0 {
            "1000000".to_owned()
        } else {
            self.keyframe_interval.to_string()
        };
        let mut command = ProcessCommand::new(&self.ffmpeg);
        command
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-vaapi_device")
            .arg(&self.device)
            .arg("-f")
            .arg("rawvideo")
            .arg("-pixel_format")
            .arg("yuv420p")
            .arg("-video_size")
            .arg(format!("{}x{}", self.coded_width, self.coded_height))
            .arg("-framerate")
            .arg(self.fps.to_string())
            .arg("-color_range")
            .arg("tv");
        match self.color {
            ColorMatrix::Bt709 => {
                command
                    .arg("-colorspace")
                    .arg("bt709")
                    .arg("-color_primaries")
                    .arg("bt709")
                    .arg("-color_trc")
                    .arg("bt709");
            }
            ColorMatrix::Bt601 => {
                command
                    .arg("-colorspace")
                    .arg("bt470bg")
                    .arg("-color_primaries")
                    .arg("bt470bg")
                    .arg("-color_trc")
                    .arg("bt709");
            }
        }
        command
            .arg("-i")
            .arg("pipe:0")
            .arg("-vf")
            .arg("format=nv12,hwupload")
            .arg("-c:v")
            .arg("h264_vaapi")
            .arg("-profile:v")
            .arg("high")
            .arg("-rc_mode")
            .arg("CBR")
            .arg("-b:v")
            .arg(format!("{}k", self.bitrate_kbps))
            .arg("-maxrate")
            .arg(format!("{}k", self.bitrate_kbps))
            .arg("-bufsize")
            .arg(format!("{}k", self.bitrate_kbps))
            .arg("-g")
            .arg(gop)
            .arg("-bf")
            .arg("0")
            // One input frame must produce one access unit before the next
            // call; any internal buffering would break the Encoder contract.
            .arg("-async_depth")
            .arg("1")
            // Insert an AUD in front of every access unit. h264_vaapi's own
            // `-aud` option emits AUD-only packets after the first frame in
            // current FFmpeg builds, so the bitstream filter is used instead.
            .arg("-bsf:v")
            .arg("h264_metadata=aud=insert")
            .arg("-flush_packets")
            .arg("1")
            .arg("-an")
            .arg("-f")
            .arg("flv")
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|error| {
            ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                format!(
                    "Could not start `{}` for VAAPI encoding: {error}",
                    self.ffmpeg.to_string_lossy()
                ),
            )
            .with_details("ffmpeg-spawn".to_owned())
            .with_remediation(
                "Install ffmpeg with VAAPI support and the h264_metadata bitstream filter, or set OZMA_FFMPEG to its path.",
            )
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                "The VAAPI encoder process did not expose a stdin pipe.",
            )
            .with_details("ffmpeg-stdin".to_owned())
            .with_remediation("Report this as a bug in the VAAPI encoder sidecar.")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ApiError::new(
                codes::ENCODER_UNAVAILABLE,
                "The VAAPI encoder process did not expose a stdout pipe.",
            )
            .with_details("ffmpeg-stdout".to_owned())
            .with_remediation("Report this as a bug in the VAAPI encoder sidecar.")
        })?;
        let stderr = child.stderr.take();
        let tail = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&tail)));

        self.sidecar = Some(Sidecar {
            child,
            stdin,
            stdout,
            stderr_tail: tail,
            stderr_task,
            header_read: false,
            nal_length_size: 4,
            sps: Vec::new(),
            pps: Vec::new(),
        });
        self.restart_needed = false;
        Ok(())
    }

    fn kill_sidecar(&mut self) {
        if let Some(mut sidecar) = self.sidecar.take() {
            sidecar.stderr_task.abort();
            let _ = sidecar.child.start_kill();
        }
    }

    fn stderr_tail(&self) -> String {
        self.sidecar
            .as_ref()
            .and_then(|sidecar| sidecar.stderr_text())
            .unwrap_or_default()
    }

    fn failure(
        &self,
        message: impl Into<String>,
        details: impl Into<String>,
        remediation: impl Into<String>,
    ) -> MediaError {
        MediaError::Api(
            ApiError::new(codes::ENCODER_UNAVAILABLE, message)
                .with_details(details.into())
                .with_remediation(remediation)
                .retryable(),
        )
    }

    async fn restart_if_needed(&mut self) -> Result<(), MediaError> {
        if self.restart_needed {
            self.start_sidecar().map_err(MediaError::Api)?;
        }
        Ok(())
    }

    async fn write_i420(&mut self, data: &[u8]) -> Result<(), MediaError> {
        let Some(sidecar) = self.sidecar.as_mut() else {
            return Err(self.failure(
                "The VAAPI encoder process is not running.",
                "ffmpeg-process",
                "Retry encoding; the sidecar is restarted automatically on the next frame.",
            ));
        };
        let result = tokio::time::timeout(FRAME_TIMEOUT, async {
            sidecar.stdin.write_all(data).await?;
            sidecar.stdin.flush().await
        })
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                let tail = self.stderr_tail();
                self.kill_sidecar();
                self.restart_needed = true;
                Err(self.failure(
                    format!("The VAAPI encoder rejected a frame: {error}{tail}"),
                    "ffmpeg-write",
                    "Check that the capture frame matches the negotiated format and that the VAAPI device is healthy.",
                ))
            }
            Err(_) => {
                let tail = self.stderr_tail();
                self.kill_sidecar();
                self.restart_needed = true;
                Err(self.failure(
                    format!(
                        "The VAAPI encoder did not accept a frame within {FRAME_TIMEOUT:?}.{tail}"
                    ),
                    "ffmpeg-timeout",
                    "Check the VAAPI driver and device; the sidecar restarts on the next frame.",
                ))
            }
        }
    }

    async fn read_access_unit(&mut self) -> Result<Vec<u8>, MediaError> {
        let Some(sidecar) = self.sidecar.as_mut() else {
            return Err(self.failure(
                "The VAAPI encoder process is not running.",
                "ffmpeg-process",
                "Retry encoding; the sidecar is restarted automatically on the next frame.",
            ));
        };
        let result = sidecar.next_access_unit().await;
        match result {
            Ok((avcc, nal_length_size)) => {
                let config = AvcConfig {
                    nal_length_size,
                    sps: sidecar.sps.clone(),
                    pps: sidecar.pps.clone(),
                };
                build_annex_b_access_unit(&config, &avcc).map_err(|message| {
                    encode_failed(
                        format!("The VAAPI encoder produced an unusable access unit: {message}"),
                        "vaapi",
                    )
                })
            }
            Err(fault) => {
                let tail = self.stderr_tail();
                self.kill_sidecar();
                self.restart_needed = true;
                let message = match fault {
                    ReadFault::Timeout => format!(
                        "The VAAPI encoder did not produce an access unit within {FRAME_TIMEOUT:?}.{tail}"
                    ),
                    ReadFault::Io(error) => {
                        format!("The VAAPI encoder stream failed: {error}{tail}")
                    }
                    ReadFault::Protocol(message) => {
                        format!("The VAAPI encoder stream was invalid: {message}{tail}")
                    }
                };
                Err(self.failure(
                    message,
                    "ffmpeg-read",
                    "Check the VAAPI driver and ffmpeg build; the sidecar restarts on the next frame.",
                ))
            }
        }
    }

    fn encode_frame(&self, frame: &Frame) -> Result<Vec<u8>, MediaError> {
        let converted = convert_to_i420(frame.format(), frame.data()).map_err(|message| {
            encode_failed(
                format!(
                    "Encoder `{}` rejected a {} frame: {message}",
                    self.id, self.input
                ),
                self.input.pixel_format.as_str().to_owned(),
            )
        })?;
        if converted.width != self.coded_width || converted.height != self.coded_height {
            return Err(encode_failed(
                format!(
                    "Encoder `{}` converted a frame to {}x{} but was configured for {}x{}.",
                    self.id, converted.width, converted.height, self.coded_width, self.coded_height
                ),
                self.input.to_string(),
            ));
        }
        Ok(converted.data)
    }
}

impl Drop for VaapiH264Encoder {
    fn drop(&mut self) {
        // `stop().await` is the clean path. On drop, close stdin (FFmpeg flushes
        // and exits on EOF) and kill it so a panic cannot leave an orphan.
        if let Some(mut sidecar) = self.sidecar.take() {
            sidecar.stderr_task.abort();
            drop(sidecar.stdin);
            let _ = sidecar.child.start_kill();
        }
    }
}

#[async_trait]
impl ozma_media::Encoder for VaapiH264Encoder {
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

        if self.stopped {
            return Err(MediaError::encode(self.id.as_str(), "encoder is closed"));
        }

        // A forced keyframe is implemented by starting a fresh VAAPI process:
        // its first frame is always an IDR. The same restart applies a changed
        // keyframe interval. A missing sidecar (a previous I/O failure) is
        // restarted here too, so one bad frame does not wedge the encoder.
        if (self.force_keyframe && self.encoded > 0) || self.sidecar.is_none() {
            self.restart_needed = true;
        }
        self.force_keyframe = false;
        self.restart_if_needed().await?;

        let data = self.encode_frame(&frame)?;
        self.write_i420(&data).await?;
        let annex_b = self.read_access_unit().await?;

        let keyframe = annex_b_is_keyframe(&annex_b);
        let output = EncodedFrame::new(
            self.output.clone(),
            pts,
            sequence,
            keyframe,
            Bytes::from(annex_b),
        )?;

        self.encoded = self.encoded.saturating_add(1);
        self.last_pts = Some(pts);
        self.last_sequence = Some(sequence);
        Ok(output)
    }

    async fn request_keyframe(&mut self) -> Result<(), MediaError> {
        if self.stopped {
            return Err(MediaError::encode(self.id.as_str(), "encoder is closed"));
        }
        self.force_keyframe = true;
        Ok(())
    }

    async fn flush(&mut self) -> Result<Vec<EncodedFrame>, MediaError> {
        // `-async_depth 1` makes the sidecar synchronous: every submitted
        // frame has already produced exactly one access unit.
        if self.stopped {
            return Err(MediaError::encode(self.id.as_str(), "encoder is closed"));
        }
        Ok(Vec::new())
    }
}

fn validate_request(input: &FrameFormat, request: &MediaFormat) -> Result<(), ApiError> {
    if request.codec != Codec::H264 {
        return Err(ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!(
                "The VAAPI backend produces H.264 only; `{}` was requested.",
                request.codec.as_str()
            ),
        )
        .with_details(request.codec.as_str().to_owned())
        .with_remediation("Request H.264, or use a backend that supports the requested codec."));
    }
    if request.hdr || request.color.is_hdr() {
        return Err(ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            "The VAAPI H.264 backend supports SDR (8-bit 4:2:0) only.",
        )
        .with_details("hdr".to_owned())
        .with_remediation("Request an SDR format, or add a 10-bit/HEVC backend for HDR capture."));
    }
    if input.width == 0 || input.height == 0 {
        return Err(ApiError::invalid(format!(
            "The encoder input format must have non-zero dimensions, got {}x{}.",
            input.width, input.height
        )));
    }
    if !matches!(
        input.pixel_format,
        PixelFormat::Mjpeg
            | PixelFormat::Nv12
            | PixelFormat::Yuyv
            | PixelFormat::Rgb24
            | PixelFormat::Bgra
    ) {
        return Err(ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!(
                "The VAAPI H.264 encoder cannot convert `{}` input.",
                input.pixel_format
            ),
        )
        .with_details(input.pixel_format.as_str().to_owned())
        .with_remediation(
            "Capture MJPEG, NV12, YUYV, RGB24, or BGRA; compressed H.264 is not this backend's input.",
        ));
    }
    Ok(())
}

fn no_device_error() -> ApiError {
    ApiError::new(
        codes::ENCODER_UNAVAILABLE,
        "No DRM render node is available for VAAPI encoding.",
    )
    .with_details("vaapi-device".to_owned())
    .with_remediation(
        "Install a VAAPI driver (for example intel-media-va-driver or mesa), expose /dev/dri/renderD128 to the service, add its user to the render group, or set OZMA_VAAPI_DEVICE.",
    )
}

/// A probe that really initialises the device and encodes a frame with
/// `h264_vaapi`.
///
/// The probe never accepts a software encode: it constructs
/// [`VaapiH264Encoder`], which can only run the `h264_vaapi` encoder over a
/// real render node, and validates the returned Annex-B access unit (start
/// code, SPS, and IDR). Any failure is recorded with a stable code and a
/// remediation.
#[derive(Clone, Debug, Default)]
pub struct VaapiProbe {
    device: Option<PathBuf>,
    checked_at: u64,
}

impl VaapiProbe {
    /// A probe against the auto-detected render node.
    pub fn new() -> Self {
        Self::default()
    }

    /// A probe against an explicit render node.
    pub fn with_device(mut self, device: impl Into<PathBuf>) -> Self {
        self.device = Some(device.into());
        self
    }

    /// Set the timestamp recorded in every result.
    pub fn with_checked_at(mut self, checked_at: u64) -> Self {
        self.checked_at = checked_at;
        self
    }

    fn now(&self) -> u64 {
        if self.checked_at != 0 {
            self.checked_at
        } else {
            unix_now()
        }
    }
}

#[async_trait]
impl EncoderProbe for VaapiProbe {
    async fn probe(&self, capability: &EncodeCapability) -> ProbeResult {
        if capability.codec != Codec::H264 || capability.backend != EncoderBackend::Vaapi {
            return ProbeResult::failed(
                capability.clone(),
                ApiError::new(
                    codes::ENCODER_PROBE_FAILED,
                    format!(
                        "The VAAPI probe validates H.264 (vaapi) only, not {} ({}).",
                        capability.codec.as_str(),
                        crate::registry::backend_name(capability.backend)
                    ),
                )
                .with_details(capability.codec.as_str().to_owned())
                .with_remediation("Use a backend-specific probe for this capability."),
                self.now(),
            );
        }
        if capability.hdr {
            return ProbeResult::failed(
                capability.clone(),
                ApiError::new(
                    codes::ENCODER_PROBE_FAILED,
                    "The VAAPI H.264 backend cannot encode HDR.".to_owned(),
                )
                .with_details("vaapi-hdr".to_owned())
                .with_remediation("Probe an SDR capability, or add a 10-bit/HEVC backend."),
                self.now(),
            );
        }

        match self.encode_probe_frame(capability).await {
            Ok(()) => ProbeResult::passed(capability.clone(), self.now()),
            Err(error) => ProbeResult::failed(capability.clone(), error, self.now()),
        }
    }
}

impl VaapiProbe {
    async fn encode_probe_frame(&self, capability: &EncodeCapability) -> Result<(), ApiError> {
        let device = match (&self.device, detect_vaapi_device()) {
            (Some(device), _) => device.clone(),
            (None, Some(device)) => device,
            (None, None) => {
                let error = no_device_error();
                return Err(ApiError::new(codes::ENCODER_PROBE_FAILED, error.message)
                    .with_details("vaapi-device".to_owned())
                    .with_remediation(error.remediation.unwrap_or_else(|| {
                        "Install a VAAPI driver and expose /dev/dri/renderD128.".to_owned()
                    }))
                    .retryable());
            }
        };

        let max_width = if capability.max_width == 0 {
            320
        } else {
            capability.max_width
        };
        let max_height = if capability.max_height == 0 {
            240
        } else {
            capability.max_height
        };
        // 320x240 exercises a real slice without spending a frame of 1080p
        // time on every node start. Clamp to the advertised envelope.
        let width = max_width.clamp(2, 320) & !1;
        let height = max_height.clamp(2, 240) & !1;
        if width < 16 || height < 16 {
            return Err(ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                format!(
                    "The capability's {}x{} limit is too small for a VAAPI probe.",
                    max_width, max_height
                ),
            )
            .with_details("vaapi-dimensions".to_owned())
            .with_remediation(
                "Advertise at least 16x16 pixels, or do not advertise this encoder.",
            ));
        }

        let fps = if capability.max_fps == 0 {
            30
        } else {
            capability.max_fps.min(30)
        };
        let input = FrameFormat::rgb24(width, height);
        let request = MediaFormat::new(Codec::H264, width, height, fps, 1_000);
        let mut encoder = VaapiH264Encoder::new_on(device.clone(), input, request).map_err(|error| {
            ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                format!(
                    "VAAPI could not initialise on `{}`: {}",
                    device.display(),
                    error.message
                ),
            )
            .with_details("vaapi-init".to_owned())
            .with_remediation(
                "Check that the libva driver supports H.264 encoding (vainfo), that /dev/dri is accessible to this user, and that ffmpeg has h264_vaapi.",
            )
            .retryable()
        })?;

        let frame =
            Frame::with_data(input, 0, 0, true, probe_pattern(width, height)).map_err(|error| {
                ApiError::new(
                    codes::ENCODER_PROBE_FAILED,
                    format!("Could not build the VAAPI probe frame: {error}"),
                )
                .with_details("vaapi-probe-frame".to_owned())
                .with_remediation("Report this as a bug in the VAAPI probe.")
            })?;

        let encoded = encoder.encode(frame).await.map_err(|error| {
            ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                format!("The VAAPI probe frame did not encode: {error}"),
            )
            .with_details("vaapi-encode".to_owned())
            .with_remediation(
                "Run `vainfo` and `ffmpeg -vaapi_device /dev/dri/renderD128 ... -c:v h264_vaapi` to inspect the driver failure.",
            )
            .retryable()
        })?;

        let types = annex_b_nal_types(encoded.data());
        if types.is_empty() {
            return Err(ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                "The VAAPI encoder produced output without an Annex-B start code.".to_owned(),
            )
            .with_details("vaapi-annexb".to_owned())
            .with_remediation("Check the ffmpeg build and the FLV access-unit framing."));
        }
        if !annex_b_is_keyframe(encoded.data()) || !types.contains(&7) {
            return Err(ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                format!(
                    "The VAAPI probe frame was not a decodable IDR with SPS (NAL types {types:?})."
                ),
            )
            .with_details("vaapi-keyframe".to_owned())
            .with_remediation(
                "Check the h264_vaapi encoder and the h264_metadata bitstream filter; the first frame must be an IDR.",
            ));
        }

        encoder.stop().await.map_err(|error| {
            ApiError::new(
                codes::ENCODER_PROBE_FAILED,
                format!("The VAAPI probe encoder did not stop cleanly: {error}"),
            )
            .with_details("vaapi-stop".to_owned())
            .with_remediation("Check for a stuck ffmpeg process and VAAPI driver.")
        })?;
        Ok(())
    }
}

/// Unix time in seconds, for probe results.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn probe_pattern(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize) * 3);
    for y in 0..height {
        for x in 0..width {
            pixels.push(((x * 4) & 0xFF) as u8);
            pixels.push(((y * 4) & 0xFF) as u8);
            pixels.push((((x + y) * 2) & 0xFF) as u8);
        }
    }
    pixels
}

// ---------------------------------------------------------------------------
// FLV tag framing and AVCC -> Annex-B conversion
// ---------------------------------------------------------------------------

const FLV_HEADER_BYTES: usize = 9;
const FLV_TAG_HEADER_BYTES: usize = 11;
const FLV_PREVIOUS_TAG_SIZE_BYTES: usize = 4;
const FLV_TAG_VIDEO: u8 = 9;
const FLV_VIDEO_CODEC_AVC: u8 = 7;
const AVC_PACKET_SEQUENCE_HEADER: u8 = 0;
const AVC_PACKET_NALU: u8 = 1;
const AVC_PACKET_END_OF_SEQUENCE: u8 = 2;

/// A parsed FLV tag. The payload is length-prefixed by the tag header.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
struct FlvTag {
    tag_type: u8,
    data: Vec<u8>,
}

/// Parse exactly one FLV tag from the front of `buffer`.
///
/// Returns the tag and the remaining bytes. This is pure so the framing can be
/// tested without a child process.
#[cfg(test)]
fn parse_flv_tag(buffer: &[u8]) -> Result<(FlvTag, &[u8]), String> {
    if buffer.len() < FLV_TAG_HEADER_BYTES + FLV_PREVIOUS_TAG_SIZE_BYTES {
        return Err(format!(
            "need at least {} bytes for an FLV tag header, got {}",
            FLV_TAG_HEADER_BYTES + FLV_PREVIOUS_TAG_SIZE_BYTES,
            buffer.len()
        ));
    }
    let tag_type = buffer[0];
    let data_size =
        (usize::from(buffer[1]) << 16) | (usize::from(buffer[2]) << 8) | usize::from(buffer[3]);
    if data_size > MAX_TAG_BYTES {
        return Err(format!(
            "FLV tag size {data_size} exceeds the {MAX_TAG_BYTES} byte limit"
        ));
    }
    let end = FLV_TAG_HEADER_BYTES
        .checked_add(data_size)
        .ok_or_else(|| "FLV tag size overflows usize".to_owned())?;
    let total = end
        .checked_add(FLV_PREVIOUS_TAG_SIZE_BYTES)
        .ok_or_else(|| "FLV tag size overflows usize".to_owned())?;
    if buffer.len() < total {
        return Err(format!(
            "FLV tag needs {total} bytes, only {} are buffered",
            buffer.len()
        ));
    }
    Ok((
        FlvTag {
            tag_type,
            data: buffer[FLV_TAG_HEADER_BYTES..end].to_vec(),
        },
        &buffer[total..],
    ))
}

/// The SPS/PPS and NAL length size from an AVC decoder configuration record.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AvcConfig {
    nal_length_size: usize,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

impl AvcConfig {
    /// Parse the `AVCDecoderConfigurationRecord` carried by an FLV sequence
    /// header tag.
    fn parse(record: &[u8]) -> Result<Self, String> {
        if record.len() < 7 {
            return Err(format!(
                "AVC configuration record is {} bytes, need at least 7",
                record.len()
            ));
        }
        let nal_length_size = usize::from(record[4] & 0x03) + 1;
        let sps_count = usize::from(record[5] & 0x1f);
        let mut offset = 6;
        let mut sps = Vec::with_capacity(sps_count);
        for _ in 0..sps_count {
            let (nal, next) = read_length_prefixed(record, offset, "SPS")?;
            sps.push(nal.to_vec());
            offset = next;
        }
        if offset >= record.len() {
            return Err("AVC configuration record is missing its PPS count".to_owned());
        }
        let pps_count = usize::from(record[offset]);
        offset += 1;
        let mut pps = Vec::with_capacity(pps_count);
        for _ in 0..pps_count {
            let (nal, next) = read_length_prefixed(record, offset, "PPS")?;
            pps.push(nal.to_vec());
            offset = next;
        }
        if sps.is_empty() || pps.is_empty() {
            return Err("AVC configuration record has no SPS/PPS".to_owned());
        }
        Ok(Self {
            nal_length_size,
            sps,
            pps,
        })
    }
}

fn read_length_prefixed<'a>(
    data: &'a [u8],
    offset: usize,
    what: &str,
) -> Result<(&'a [u8], usize), String> {
    if offset + 2 > data.len() {
        return Err(format!("truncated {what} length"));
    }
    let length = usize::from(data[offset]) << 8 | usize::from(data[offset + 1]);
    let start = offset + 2;
    let end = start
        .checked_add(length)
        .ok_or_else(|| format!("{what} length overflows usize"))?;
    if end > data.len() {
        return Err(format!(
            "{what} needs {length} bytes, only {} remain",
            data.len().saturating_sub(start)
        ));
    }
    Ok((&data[start..end], end))
}

/// Split AVCC length-prefixed NAL units out of one access unit.
fn avcc_nals(data: &[u8], nal_length_size: usize) -> Result<Vec<&[u8]>, String> {
    if !(1..=4).contains(&nal_length_size) {
        return Err(format!("invalid NAL length size {nal_length_size}"));
    }
    let mut nals = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        if offset + nal_length_size > data.len() {
            return Err(format!(
                "truncated NAL length at byte {offset} of {}",
                data.len()
            ));
        }
        let mut length = 0usize;
        for byte in &data[offset..offset + nal_length_size] {
            length = (length << 8) | usize::from(*byte);
        }
        offset += nal_length_size;
        if length == 0 {
            return Err(format!("zero-length NAL at byte {offset}"));
        }
        let end = offset
            .checked_add(length)
            .ok_or_else(|| "NAL length overflows usize".to_owned())?;
        if end > data.len() {
            return Err(format!(
                "NAL at byte {offset} needs {length} bytes, only {} remain",
                data.len().saturating_sub(offset)
            ));
        }
        nals.push(&data[offset..end]);
        offset = end;
    }
    if nals.is_empty() {
        return Err("access unit contains no NAL units".to_owned());
    }
    Ok(nals)
}

/// Convert one AVCC access unit to Annex-B, adding missing SPS/PPS on
/// keyframes and rejecting units without a slice.
///
/// The bitstream filter normally inserts an AUD and `h264_vaapi` repeats
/// SPS/PPS on every IDR, so the insertion path is a safety net for streams
/// whose sequence header changes mid-flight.
fn build_annex_b_access_unit(config: &AvcConfig, avcc: &[u8]) -> Result<Vec<u8>, String> {
    let nals = avcc_nals(avcc, config.nal_length_size)?;
    let types: Vec<u8> = nals.iter().map(|nal| nal[0] & 0x1f).collect();
    let keyframe = types.contains(&5);
    if !types.iter().any(|nal_type| (1..=5).contains(nal_type)) {
        return Err(format!(
            "access unit has no VCL slice (NAL types {types:?})"
        ));
    }

    let starts_with_aud = types.first() == Some(&9);
    let mut ordered: Vec<Vec<u8>> = Vec::with_capacity(nals.len() + 2);
    for nal in &nals {
        ordered.push(nal.to_vec());
    }

    if keyframe && (!types.contains(&7) || !types.contains(&8)) {
        let mut insert_at = usize::from(starts_with_aud);
        if !types.contains(&7) {
            for sps in &config.sps {
                ordered.insert(insert_at, sps.clone());
                insert_at += 1;
            }
        }
        if !types.contains(&8) {
            for pps in &config.pps {
                ordered.insert(insert_at, pps.clone());
                insert_at += 1;
            }
        }
    }

    let mut out = Vec::with_capacity(avcc.len() + 32 * ordered.len());
    for nal in ordered {
        if nal.is_empty() {
            return Err("empty NAL unit".to_owned());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&nal);
    }
    Ok(out)
}

enum ReadFault {
    Timeout,
    Io(io::Error),
    Protocol(String),
}

impl From<io::Error> for ReadFault {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct Sidecar {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    stderr_task: JoinHandle<()>,
    header_read: bool,
    nal_length_size: usize,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
}

impl Sidecar {
    fn stderr_text(&self) -> Option<String> {
        let tail = self.stderr_tail.lock().ok()?;
        if tail.is_empty() {
            return None;
        }
        Some(format!(
            "\nffmpeg stderr: {}",
            String::from_utf8_lossy(&tail).trim()
        ))
    }

    async fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), ReadFault> {
        match tokio::time::timeout(FRAME_TIMEOUT, self.stdout.read_exact(buffer)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(ReadFault::Io(error)),
            Err(_) => Err(ReadFault::Timeout),
        }
    }

    async fn ensure_header(&mut self) -> Result<(), ReadFault> {
        if self.header_read {
            return Ok(());
        }
        let mut header = [0u8; FLV_HEADER_BYTES];
        self.read_exact(&mut header).await?;
        if &header[..3] != b"FLV" {
            return Err(ReadFault::Protocol(format!(
                "expected an FLV header, got {:02x?}",
                &header[..3]
            )));
        }
        let mut previous = [0u8; FLV_PREVIOUS_TAG_SIZE_BYTES];
        self.read_exact(&mut previous).await?;
        self.header_read = true;
        Ok(())
    }

    /// Read the next access unit as AVCC NAL units plus the NAL length size.
    async fn next_access_unit(&mut self) -> Result<(Vec<u8>, usize), ReadFault> {
        self.ensure_header().await?;
        for _ in 0..MAX_TAGS_PER_FRAME {
            let mut header = [0u8; FLV_TAG_HEADER_BYTES];
            self.read_exact(&mut header).await?;
            let tag_type = header[0];
            let data_size = (usize::from(header[1]) << 16)
                | (usize::from(header[2]) << 8)
                | usize::from(header[3]);
            if data_size > MAX_TAG_BYTES {
                return Err(ReadFault::Protocol(format!(
                    "FLV tag size {data_size} exceeds the {MAX_TAG_BYTES} byte limit"
                )));
            }
            let mut data = vec![0u8; data_size];
            self.read_exact(&mut data).await?;
            let mut previous = [0u8; FLV_PREVIOUS_TAG_SIZE_BYTES];
            self.read_exact(&mut previous).await?;

            if tag_type != FLV_TAG_VIDEO || data.len() < 5 {
                continue;
            }
            if data[0] & 0x0f != FLV_VIDEO_CODEC_AVC {
                return Err(ReadFault::Protocol(format!(
                    "expected AVC video, got FLV codec id {}",
                    data[0] & 0x0f
                )));
            }
            match data[1] {
                AVC_PACKET_SEQUENCE_HEADER => {
                    let config = AvcConfig::parse(&data[5..])
                        .map_err(|message| ReadFault::Protocol(message))?;
                    self.nal_length_size = config.nal_length_size;
                    self.sps = config.sps;
                    self.pps = config.pps;
                }
                AVC_PACKET_NALU => {
                    return Ok((data[5..].to_vec(), self.nal_length_size));
                }
                AVC_PACKET_END_OF_SEQUENCE => continue,
                other => {
                    return Err(ReadFault::Protocol(format!(
                        "unknown AVC packet type {other}"
                    )))
                }
            }
        }
        Err(ReadFault::Protocol(format!(
            "no video access unit in {MAX_TAGS_PER_FRAME} FLV tags"
        )))
    }
}

async fn drain_stderr<R: AsyncRead + Unpin>(stderr: Option<R>, tail: Arc<Mutex<Vec<u8>>>) {
    let Some(stderr) = stderr else {
        return;
    };
    let mut reader = BufReader::new(stderr);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if let Ok(mut tail) = tail.lock() {
                    tail.extend_from_slice(&chunk[..read]);
                    if tail.len() > STDERR_TAIL_BYTES {
                        let drain = tail.len() - STDERR_TAIL_BYTES;
                        tail.drain(..drain);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::software::annex_b_nal_types;

    fn build_config(sps: &[u8], pps: &[u8], nal_length_size: usize) -> Vec<u8> {
        let mut record = vec![
            1,
            sps.get(1).copied().unwrap_or(0x64),
            0,
            sps.get(3).copied().unwrap_or(0x0d),
            0xfc | (nal_length_size as u8 - 1),
            0xe0 | 1,
        ];
        record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        record.extend_from_slice(sps);
        record.push(1);
        record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        record.extend_from_slice(pps);
        record
    }

    fn avcc(nals: &[&[u8]], length_size: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for nal in nals {
            let length = nal.len() as u32;
            out.extend_from_slice(&length.to_be_bytes()[4 - length_size..]);
            out.extend_from_slice(nal);
        }
        out
    }

    #[test]
    fn avc_config_record_is_parsed() {
        let sps = [0x67, 0x64, 0x00, 0x0d, 0x01];
        let pps = [0x68, 0xee, 0x38, 0x80];
        let record = build_config(&sps, &pps, 4);
        let config = AvcConfig::parse(&record).unwrap();
        assert_eq!(config.nal_length_size, 4);
        assert_eq!(config.sps, vec![sps.to_vec()]);
        assert_eq!(config.pps, vec![pps.to_vec()]);
    }

    #[test]
    fn avc_config_rejects_truncated_records() {
        assert!(AvcConfig::parse(&[1, 2, 3]).is_err());
        let record = build_config(&[0x67, 0x64, 0x00, 0x0d], &[0x68, 0xee], 4);
        assert!(AvcConfig::parse(&record[..record.len() - 1]).is_err());
    }

    #[test]
    fn avcc_keyframe_becomes_annex_b_with_sps_and_pps() {
        let sps = [0x67, 0x64, 0x00, 0x0d, 0x01];
        let pps = [0x68, 0xee, 0x38, 0x80];
        let record = build_config(&sps, &pps, 4);
        let config = AvcConfig::parse(&record).unwrap();
        let au = avcc(&[&[0x09, 0x10], &[0x65, 0x88, 0x84]], 4);
        let annex_b = build_annex_b_access_unit(&config, &au).unwrap();
        assert!(annex_b.starts_with(&[0, 0, 0, 1]));
        assert_eq!(annex_b_nal_types(&annex_b), vec![9, 7, 8, 5]);
        assert!(annex_b_is_keyframe(&annex_b));
    }

    #[test]
    fn avcc_inter_frame_does_not_gain_parameter_sets() {
        let sps = [0x67, 0x64, 0x00, 0x0d, 0x01];
        let pps = [0x68, 0xee, 0x38, 0x80];
        let record = build_config(&sps, &pps, 4);
        let config = AvcConfig::parse(&record).unwrap();
        let au = avcc(&[&[0x09, 0x30], &[0x41, 0x9a, 0x00]], 4);
        let annex_b = build_annex_b_access_unit(&config, &au).unwrap();
        assert_eq!(annex_b_nal_types(&annex_b), vec![9, 1]);
        assert!(!annex_b_is_keyframe(&annex_b));
    }

    #[test]
    fn avcc_without_a_slice_is_rejected() {
        let sps = [0x67, 0x64, 0x00, 0x0d, 0x01];
        let pps = [0x68, 0xee, 0x38, 0x80];
        let config = AvcConfig::parse(&build_config(&sps, &pps, 4)).unwrap();
        let au = avcc(&[&[0x09, 0x10], &[0x06, 0x05, 0x01]], 4);
        assert!(build_annex_b_access_unit(&config, &au).is_err());
    }

    #[test]
    fn avcc_rejects_bad_lengths() {
        let sps = [0x67, 0x64, 0x00, 0x0d, 0x01];
        let pps = [0x68, 0xee, 0x38, 0x80];
        let config = AvcConfig::parse(&build_config(&sps, &pps, 4)).unwrap();
        // NAL length says 16 bytes but only 2 follow.
        let mut au = 16u32.to_be_bytes().to_vec();
        au.extend_from_slice(&[0x65, 0x88]);
        assert!(build_annex_b_access_unit(&config, &au).is_err());
    }

    #[test]
    fn flv_tags_are_length_prefixed() {
        let payload = [0x17, 0x01, 0, 0, 0, 0x00, 0x00, 0x00, 0x02, 0x65, 0x01];
        let mut stream = Vec::new();
        stream.push(FLV_TAG_VIDEO);
        stream.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        stream.extend_from_slice(&[0, 0, 0, 0]);
        stream.extend_from_slice(&[0, 0, 0]);
        stream.extend_from_slice(&payload);
        stream.extend_from_slice(&[0, 0, 0, 0xFF]);
        // A second tag proves the first consumed exactly its declared length.
        stream.push(18);
        stream.extend_from_slice(&[0, 0, 3, 0, 0, 0, 0, 0, 0, 0]);
        stream.extend_from_slice(&[1, 2, 3]);
        stream.extend_from_slice(&[0, 0, 0, 1]);

        let (tag, rest) = parse_flv_tag(&stream).unwrap();
        assert_eq!(tag.tag_type, FLV_TAG_VIDEO);
        assert_eq!(tag.data, payload);
        let (second, rest) = parse_flv_tag(rest).unwrap();
        assert_eq!(second.tag_type, 18);
        assert_eq!(second.data, vec![1, 2, 3]);
        assert!(rest.is_empty());
    }

    #[test]
    fn flv_tag_parser_rejects_a_partial_tag() {
        let mut stream = vec![FLV_TAG_VIDEO, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0];
        stream.extend_from_slice(&[0; 4]);
        assert!(parse_flv_tag(&stream).is_err());
    }

    #[test]
    fn device_detection_honours_the_environment() {
        // The render node may or may not exist in CI; detection must never
        // panic, and an explicit missing override must not be reported.
        let detected = detect_vaapi_device();
        if let Some(path) = detected {
            assert!(path.starts_with("/dev/dri") || std::env::var_os(VAAPI_DEVICE_ENV).is_some());
        }
    }

    #[test]
    fn capability_is_hardware_h264() {
        let capability = VaapiH264Encoder::capability();
        assert_eq!(capability.codec, Codec::H264);
        assert_eq!(capability.backend, EncoderBackend::Vaapi);
        assert!(capability.hardware);
        assert!(!capability.hdr);
        assert_eq!(capability.max_width, VAAPI_MAX_WIDTH);
    }

    #[test]
    fn construction_does_not_need_a_tokio_runtime() {
        // `EncoderSelection::build_encoder` is synchronous, so constructing a
        // VAAPI encoder must not spawn a task or require a reactor. This is a
        // plain `#[test]`, deliberately outside any runtime.
        let request = MediaFormat::new(Codec::H264, 160, 96, 30, 800);
        match VaapiH264Encoder::new(FrameFormat::rgb24(160, 96), request) {
            Ok(encoder) => {
                assert!(encoder.device().exists());
                assert_eq!(encoder.encoded_count(), 0);
            }
            Err(error) => {
                // CI without a render node or ffmpeg still gets a structured,
                // actionable error rather than a panic.
                assert_eq!(error.code, codes::ENCODER_UNAVAILABLE);
                assert!(error.remediation.is_some());
            }
        }
    }

    #[test]
    fn accepts_every_pipeline_raw_layout() {
        let request = MediaFormat::new(Codec::H264, 320, 240, 30, 1_000);
        for pixel_format in [
            PixelFormat::Nv12,
            PixelFormat::Yuyv,
            PixelFormat::Rgb24,
            PixelFormat::Bgra,
            PixelFormat::Mjpeg,
        ] {
            let format = FrameFormat::new(pixel_format, 320, 240);
            assert!(
                validate_request(&format, &request).is_ok(),
                "{pixel_format} should be accepted"
            );
        }
        for pixel_format in [
            PixelFormat::P010,
            PixelFormat::H264,
            PixelFormat::Hevc,
            PixelFormat::Av1,
        ] {
            let format = FrameFormat::new(pixel_format, 320, 240);
            assert!(
                validate_request(&format, &request).is_err(),
                "{pixel_format} should be rejected"
            );
        }
    }

    #[test]
    fn construction_rejects_a_missing_device_and_unsupported_input() {
        let request = MediaFormat::new(Codec::H264, 320, 240, 30, 1_000);
        let missing = VaapiH264Encoder::new_on(
            "/nonexistent/ozma/renderD999",
            FrameFormat::rgb24(320, 240),
            request.clone(),
        )
        .unwrap_err();
        assert_eq!(missing.code, codes::ENCODER_UNAVAILABLE);
        assert!(missing.remediation.is_some());

        let h264_input = FrameFormat::new(PixelFormat::H264, 320, 240);
        let unsupported = VaapiH264Encoder::new_on("/dev/null", h264_input, request).unwrap_err();
        assert_eq!(unsupported.code, codes::ENCODER_UNAVAILABLE);
        assert!(unsupported.remediation.is_some());
    }

    #[tokio::test]
    async fn probe_reports_a_missing_device() {
        let probe = VaapiProbe::new().with_device("/nonexistent/ozma/renderD999");
        let result = probe.probe(&VaapiH264Encoder::capability()).await;
        assert!(!result.is_ok());
        let error = result.error.expect("structured failure");
        assert_eq!(error.code, codes::ENCODER_PROBE_FAILED);
        assert!(error.remediation.is_some());
    }

    #[tokio::test]
    async fn probe_rejects_non_vaapi_capabilities() {
        let probe = VaapiProbe::new();
        let software = crate::software::H264SoftwareEncoder::capability();
        let result = probe.probe(&software).await;
        assert!(!result.is_ok());
        assert_eq!(result.backend(), EncoderBackend::Software);
        let error = result.error.unwrap();
        assert_eq!(error.code, codes::ENCODER_PROBE_FAILED);
    }

    fn vaapi_test_or_skip() -> bool {
        let forced = std::env::var("OZMA_TEST_VAAPI")
            .map(|value| value == "1")
            .unwrap_or(false);
        if forced {
            return true;
        }
        if detect_vaapi_device().is_some() {
            return true;
        }
        eprintln!("skipping VAAPI test: no /dev/dri/renderD* and OZMA_TEST_VAAPI is not 1");
        false
    }

    async fn probe_or_skip() -> Option<EncodeCapability> {
        let forced = std::env::var("OZMA_TEST_VAAPI")
            .map(|value| value == "1")
            .unwrap_or(false);
        let capability = VaapiH264Encoder::capability();
        let result = VaapiProbe::new()
            .with_checked_at(1)
            .probe(&capability)
            .await;
        if result.is_ok() {
            return Some(capability);
        }
        let message = result
            .error
            .as_ref()
            .map(|error| format!("{} ({})", error.message, error.code))
            .unwrap_or_else(|| "no probe error recorded".to_owned());
        if forced {
            panic!("OZMA_TEST_VAAPI=1 but the VAAPI probe failed: {message}");
        }
        eprintln!("skipping VAAPI encode test: probe failed: {message}");
        None
    }

    #[tokio::test]
    async fn vaapi_encodes_real_annex_b_access_units() {
        if !vaapi_test_or_skip() {
            return;
        }
        if probe_or_skip().await.is_none() {
            return;
        }

        let input = FrameFormat::new(PixelFormat::Nv12, 320, 240);
        let request = MediaFormat::new(Codec::H264, 320, 240, 30, 1_500);
        let mut encoder = VaapiH264Encoder::new(input, request).expect("VAAPI encoder");
        assert!(encoder.device().exists());

        let mut total = 0usize;
        let mut keyframes = Vec::new();
        let mut per_frame = Vec::new();
        let mut stream = Vec::new();
        for index in 0u64..6 {
            let width = 320usize;
            let height = 240usize;
            let mut data = vec![0u8; width * height * 3 / 2];
            for y in 0..height {
                for x in 0..width {
                    data[y * width + x] = ((x + index as usize * 9) & 0xff) as u8;
                }
            }
            for (i, value) in data[width * height..].iter_mut().enumerate() {
                *value = (96 + ((i / 2 + index as usize * 5) & 0x3f)) as u8;
            }
            if index == 3 {
                encoder.request_keyframe().await.unwrap();
            }
            let frame = Frame::with_data(input, index * 33_333, index, index == 0, data).unwrap();
            let encoded = encoder.encode(frame).await.expect("VAAPI encode");
            assert_eq!(encoded.pts_micros(), index * 33_333);
            assert_eq!(encoded.sequence(), index);
            let types = annex_b_nal_types(encoded.data());
            assert!(
                types.iter().any(|t| (1..=5).contains(t)),
                "no slice: {types:?}"
            );
            assert!(types.contains(&9), "no AUD: {types:?}");
            if index == 0 || index == 3 {
                assert!(encoded.is_keyframe(), "frame {index} was not a keyframe");
                assert!(types.contains(&7), "keyframe without SPS: {types:?}");
                assert!(types.contains(&5), "keyframe without IDR: {types:?}");
                keyframes.push(index);
            } else {
                assert!(
                    !encoded.is_keyframe(),
                    "frame {index} was unexpectedly a keyframe"
                );
            }
            total += encoded.len();
            per_frame.push(encoded.len());
            stream.extend_from_slice(encoded.data());
            assert!(!encoded.is_empty());
        }

        assert_eq!(encoder.encoded_count(), 6);
        assert!(total > 100, "payload was only {total} bytes");
        assert_eq!(encoder.output_format().codec, Codec::H264);
        assert_eq!(encoder.output_format().width, 320);
        assert_eq!(encoder.output_format().height, 240);
        let decoded = verify_with_ffmpeg(&stream, 320, 240);
        eprintln!(
            "VAAPI H.264 NV12 320x240: 6 frames, {} payload bytes, keyframes {keyframes:?}, sizes {per_frame:?}, decoded {decoded}",
            total
        );
        encoder.stop().await.expect("clean stop");
        encoder.stop().await.expect("stop must be idempotent");
        let after_stop = encoder
            .encode(
                Frame::with_data(input, 6 * 33_333, 6, false, vec![0u8; 320 * 240 * 3 / 2])
                    .unwrap(),
            )
            .await;
        assert!(
            matches!(after_stop, Err(MediaError::Encode { .. })),
            "encoding after stop must fail cleanly"
        );
    }

    #[tokio::test]
    async fn vaapi_accepts_mjpeg_input() {
        if !vaapi_test_or_skip() {
            return;
        }
        if probe_or_skip().await.is_none() {
            return;
        }

        let (width, height) = (160u16, 96u16);
        let input = FrameFormat::new(PixelFormat::Mjpeg, u32::from(width), u32::from(height));
        let request = MediaFormat::new(Codec::H264, u32::from(width), u32::from(height), 30, 800);
        let mut encoder = VaapiH264Encoder::new(input, request).expect("VAAPI encoder");

        let mut rgb = vec![0u8; usize::from(width) * usize::from(height) * 3];
        for (index, chunk) in rgb.chunks_exact_mut(3).enumerate() {
            chunk[0] = ((index * 3) & 0xff) as u8;
            chunk[1] = ((index * 5) & 0xff) as u8;
            chunk[2] = ((index * 7) & 0xff) as u8;
        }
        let mut jpeg = Vec::new();
        let encoder_settings = jpeg_encoder::Encoder::new(&mut jpeg, 90);
        encoder_settings
            .encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb)
            .expect("encode MJPEG fixture");

        let frame = Frame::with_data(input, 0, 0, true, jpeg.clone()).unwrap();
        let encoded = encoder.encode(frame).await.expect("VAAPI MJPEG encode");
        assert!(encoded.is_keyframe());
        assert!(annex_b_nal_types(encoded.data()).contains(&5));
        eprintln!(
            "VAAPI H.264 MJPEG 160x96: {} payload bytes, decoded {}",
            encoded.len(),
            verify_with_ffmpeg(encoded.data(), 160, 96)
        );
        encoder.stop().await.unwrap();
    }

    /// Decode the produced Annex-B stream with the system `ffmpeg`/`ffprobe`.
    ///
    /// This is the part that turns "the probe saw an IDR" into "the hardware
    /// bitstream is actually decodable". It is best-effort about `ffprobe`
    /// being installed, but a decode failure is a test failure.
    fn verify_with_ffmpeg(stream: &[u8], width: u32, height: u32) -> String {
        use std::process::Command;

        let path = std::env::temp_dir().join(format!(
            "ozma-vaapi-test-{}-{}.h264",
            std::process::id(),
            width
        ));
        std::fs::write(&path, stream).expect("write VAAPI test stream");

        let decode = Command::new(ffmpeg_binary())
            .args(["-v", "error", "-f", "h264", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"])
            .output();
        let _ = std::fs::remove_file(&path);
        match decode {
            Ok(output) => assert!(
                output.status.success() && output.stderr.is_empty(),
                "ffmpeg could not decode the VAAPI bitstream: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => panic!("could not run ffmpeg to verify the VAAPI bitstream: {error}"),
        }

        let path = std::env::temp_dir().join(format!(
            "ozma-vaapi-test-{}-{}.probe.h264",
            std::process::id(),
            height
        ));
        std::fs::write(&path, stream).expect("write VAAPI test stream");
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-count_frames",
                "-show_entries",
                "stream=codec_name,profile,level,nb_read_frames",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&path)
            .output();
        let _ = std::fs::remove_file(&path);
        match probe {
            Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
                .lines()
                .collect::<Vec<_>>()
                .join(" "),
            _ => "unknown (ffprobe unavailable)".to_owned(),
        }
    }

    /// Opt-in smoke test for the advertised 1080p envelope. It is heavier than
    /// the small-frame tests and only runs when VAAPI is explicitly requested,
    /// so ordinary `cargo test` on a VAAPI host stays fast.
    #[tokio::test]
    async fn vaapi_1080p30_smoke_test() {
        let forced = std::env::var("OZMA_TEST_VAAPI")
            .map(|value| value == "1")
            .unwrap_or(false);
        if !forced {
            eprintln!("skipping 1080p VAAPI smoke test: set OZMA_TEST_VAAPI=1 to run it");
            return;
        }
        if probe_or_skip().await.is_none() {
            return;
        }

        let (width, height) = (1920usize, 1080usize);
        let input = FrameFormat::new(PixelFormat::Nv12, width as u32, height as u32);
        let request = MediaFormat::new(Codec::H264, width as u32, height as u32, 30, 12_000);
        let mut encoder = VaapiH264Encoder::new(input, request).expect("VAAPI 1080p encoder");
        let mut stream = Vec::new();
        let mut sizes = Vec::new();
        for index in 0u64..3 {
            let mut data = vec![0u8; width * height * 3 / 2];
            for y in 0..height {
                for x in 0..width {
                    data[y * width + x] = ((x / 2 + y / 2 + index as usize * 13) & 0xff) as u8;
                }
            }
            let frame = Frame::with_data(input, index * 33_333, index, index == 0, data).unwrap();
            let encoded = encoder.encode(frame).await.expect("VAAPI 1080p encode");
            if index == 0 {
                assert!(encoded.is_keyframe());
                assert!(annex_b_nal_types(encoded.data()).contains(&7));
            }
            sizes.push(encoded.len());
            stream.extend_from_slice(encoded.data());
        }
        let decoded = verify_with_ffmpeg(&stream, width as u32, height as u32);
        eprintln!(
            "VAAPI H.264 1920x1080: 3 frames, {} payload bytes, sizes {sizes:?}, decoded {decoded}",
            stream.len()
        );
        encoder.stop().await.unwrap();
    }

    #[tokio::test]
    async fn vaapi_accepts_rgb24_and_reports_profile() {
        if !vaapi_test_or_skip() {
            return;
        }
        if probe_or_skip().await.is_none() {
            return;
        }

        let input = FrameFormat::rgb24(160, 96);
        let request = MediaFormat::new(Codec::H264, 160, 96, 30, 800);
        let mut encoder = VaapiH264Encoder::new(input, request).expect("VAAPI encoder");
        let mut saw_keyframe = false;
        let mut last_len = 0;
        for index in 0u64..4 {
            let mut data = vec![0u8; 160 * 96 * 3];
            for (pixel, chunk) in data.chunks_exact_mut(3).enumerate() {
                chunk[0] = ((pixel + index as usize * 3) & 0xff) as u8;
                chunk[1] = ((pixel / 2 + index as usize) & 0xff) as u8;
                chunk[2] = ((pixel / 3) & 0xff) as u8;
            }
            let frame = Frame::with_data(input, index * 33_333, index, index == 0, data).unwrap();
            let encoded = encoder.encode(frame).await.expect("VAAPI RGB24 encode");
            last_len = encoded.len();
            if encoded.is_keyframe() {
                saw_keyframe = true;
                let sps = annex_b_nal_types(encoded.data());
                assert!(sps.contains(&7));
            }
        }
        assert!(saw_keyframe);
        assert!(last_len > 0);
        encoder.stop().await.unwrap();
    }
}
