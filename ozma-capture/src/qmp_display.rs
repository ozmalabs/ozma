// SPDX-License-Identifier: AGPL-3.0-only
//! QMP `screendump` display capture.
//!
//! QEMU can serialize its emulated display to a PPM file on request over QMP.
//! This provider turns that into a [`CaptureSource`]: each
//! [`CaptureSource::next_frame`] call issues `screendump`, reads the resulting
//! binary P6 file, parses it into packed RGB24, and deletes the file.
//!
//! ## This is a polling path
//!
//! `screendump` is not a framebuffer subscription and is not zero-copy or
//! real-time. Every frame costs a QMP round trip plus a full write/read of the
//! framebuffer through the filesystem, so latency is at least one screendump
//! duration and throughput is bounded by [`QmpDisplayConfig::max_fps`]. The
//! provider sleeps only for the remainder of the frame interval; when a
//! screendump already took longer than the interval it does not add extra
//! delay. The D-Bus framebuffer provider is the lower-latency VM path in the
//! reconciliation plan.
//!
//! ## Connection lifecycle
//!
//! [`QmpDisplayProvider::enumerate`] only checks that the socket path exists;
//! it never connects. [`QmpDisplayProvider::open`] is synchronous and also
//! does not connect. [`QmpDisplaySource`] connects (and performs the
//! `qmp_capabilities` handshake) lazily on the first call to `next_frame`, then
//! reuses that control socket. A missing or unresponsive socket therefore
//! surfaces as a typed [`MediaError`] from `next_frame`, not from `open`.
//!
//! ## Temporary files
//!
//! Every frame uses a unique `ozma-qmp-<pid>-<counter>.ppm` file in the
//! configured [`QmpDisplayConfig::screendump_path`] directory (the system temp
//! directory when it is `None`). The file is removed after it has been read;
//! a failed removal is logged but does not fail the frame. The file is visible
//! to other local users subject to the temp directory's permissions.
//!
//! ## Multi-display
//!
//! `ozma-qmp`'s `screendump` call does not pass a `device` argument, so this
//! provider captures QEMU's default console. Per-console selection needs a
//! QMP arguments extension and is deferred to the multi-display workstream.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use ozma_qmp::{QmpClient, QmpConfig, QmpError};
use tracing::warn;

use crate::provider::CaptureProvider;
use crate::v4l2::{monotonic_pts, next_sequence};

/// Default source id (`qmp0`).
pub const QMP_DEFAULT_ID: &str = "qmp0";

/// Default human-readable source name.
pub const QMP_DEFAULT_NAME: &str = "QEMU VM display";

/// Default advertised maximum width.
pub const QMP_DEFAULT_MAX_WIDTH: u32 = 1920;

/// Default advertised maximum height.
pub const QMP_DEFAULT_MAX_HEIGHT: u32 = 1080;

/// Default polling/frame-rate ceiling.
pub const QMP_DEFAULT_MAX_FPS: u32 = 30;

/// Largest width or height [`parse_ppm_p6`] accepts.
///
/// This is a sanity bound against corrupt or hostile headers, not a product
/// limit. It matches the Looking Glass KVMFR parser's bound.
pub const MAX_PPM_DIMENSION: u32 = 16_384;

/// Process-wide counter for unique screendump filenames.
static SCREENDUMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Configuration for [`QmpDisplayProvider`] and [`QmpDisplaySource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QmpDisplayConfig {
    /// QEMU control QMP Unix socket.
    pub qmp_socket: PathBuf,
    /// Stable capture source id.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Largest width this source will accept from a screendump.
    pub max_width: u32,
    /// Largest height this source will accept from a screendump.
    pub max_height: u32,
    /// Upper bound on frames per second (must be non-zero).
    pub max_fps: u32,
    /// Directory for temporary PPM files.
    ///
    /// `None` means the system temp directory. The directory is created on
    /// demand before the first screendump.
    pub screendump_path: Option<PathBuf>,
}

impl Default for QmpDisplayConfig {
    fn default() -> Self {
        Self {
            qmp_socket: PathBuf::from("/run/ozma/qmp0.sock"),
            id: QMP_DEFAULT_ID.to_owned(),
            name: QMP_DEFAULT_NAME.to_owned(),
            max_width: QMP_DEFAULT_MAX_WIDTH,
            max_height: QMP_DEFAULT_MAX_HEIGHT,
            max_fps: QMP_DEFAULT_MAX_FPS,
            screendump_path: None,
        }
    }
}

impl QmpDisplayConfig {
    /// Configuration for a specific QMP socket, with default identity and
    /// limits.
    pub fn new(qmp_socket: impl Into<PathBuf>) -> Self {
        Self {
            qmp_socket: qmp_socket.into(),
            ..Self::default()
        }
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.id = id.into();
        self.name = name.into();
        self
    }

    /// Override the accepted screendump dimensions.
    pub fn with_limits(mut self, max_width: u32, max_height: u32) -> Self {
        self.max_width = max_width;
        self.max_height = max_height;
        self
    }

    /// Override the frame-rate ceiling.
    pub fn with_max_fps(mut self, max_fps: u32) -> Self {
        self.max_fps = max_fps;
        self
    }

    /// Set the directory used for temporary PPM files.
    pub fn with_screendump_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.screendump_path = Some(directory.into());
        self
    }

    /// Directory used for temporary PPM files.
    pub fn screendump_dir(&self) -> PathBuf {
        self.screendump_path
            .clone()
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Frame interval derived from [`QmpDisplayConfig::max_fps`].
    pub fn frame_interval(&self) -> Duration {
        Duration::from_micros((1_000_000 / u64::from(self.max_fps.max(1))).max(1))
    }

    fn validate(&self) -> Result<(), ApiError> {
        if self.qmp_socket.as_os_str().is_empty() {
            return Err(ApiError::invalid(
                "The QMP display source requires a non-empty QMP socket path.",
            ));
        }
        if self.id.trim().is_empty() {
            return Err(ApiError::invalid(
                "The QMP display source requires a non-empty id.",
            ));
        }
        if self.max_width == 0 || self.max_height == 0 {
            return Err(ApiError::invalid(
                "QMP display limits must have non-zero width and height.",
            ));
        }
        if self.max_width > MAX_PPM_DIMENSION || self.max_height > MAX_PPM_DIMENSION {
            return Err(ApiError::invalid(format!(
                "QMP display limits {}x{} exceed the {MAX_PPM_DIMENSION}-pixel PPM bound.",
                self.max_width, self.max_height
            )));
        }
        if self.max_fps == 0 {
            return Err(ApiError::invalid("QMP display max_fps must be at least 1."));
        }
        Ok(())
    }
}

/// A capture provider that reads QEMU displays through QMP `screendump`.
#[derive(Clone, Debug)]
pub struct QmpDisplayProvider {
    config: QmpDisplayConfig,
}

impl QmpDisplayProvider {
    /// Create a provider for `config`.
    pub fn new(config: QmpDisplayConfig) -> Self {
        Self { config }
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &QmpDisplayConfig {
        &self.config
    }

    /// The capability advertised when the QMP socket path exists.
    ///
    /// Returns `None` without touching the socket when the path is missing;
    /// discovery never connects to QEMU.
    pub fn capability(&self) -> Option<CaptureCapability> {
        if !self.config.qmp_socket.exists() {
            return None;
        }

        Some(CaptureCapability {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            kind: CaptureKind::Qmp,
            formats: vec![PixelFormat::Rgb24.as_str().to_owned()],
            max_width: self.config.max_width,
            max_height: self.config.max_height,
            max_fps: self.config.max_fps.max(1),
            compressed: false,
            hdr: false,
            has_audio: false,
        })
    }
}

impl CaptureProvider for QmpDisplayProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::Qmp
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(self.capability().into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::Qmp || capability.id != self.config.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("QMP display source `{}` is not available.", capability.id),
            )
            .with_details(capability.id.clone()));
        }

        if !self.config.qmp_socket.exists() {
            return Err(missing_socket_error(&self.config.qmp_socket));
        }

        Ok(Box::new(QmpDisplaySource::new(self.config.clone())?))
    }
}

/// A [`CaptureSource`] that polls QEMU's display with QMP `screendump`.
///
/// The QMP connection is opened lazily by the first `next_frame` call. Frames
/// are owned RGB24 buffers; no QEMU memory is aliased into the pipeline.
#[derive(Debug)]
pub struct QmpDisplaySource {
    config: QmpDisplayConfig,
    client: QmpClient,
    format: FrameFormat,
    interval: Duration,
    origin: Instant,
    stopped: bool,
    last_sequence: Option<u64>,
    last_pts: Option<u64>,
}

impl QmpDisplaySource {
    /// Build a source for `config` without connecting.
    ///
    /// Rejects empty paths/ids, zero or oversized dimensions, and a zero
    /// frame-rate ceiling with a structured [`ApiError`]. The socket itself is
    /// only opened on the first `next_frame`.
    pub fn new(config: QmpDisplayConfig) -> Result<Self, ApiError> {
        config.validate()?;
        let interval = config.frame_interval();
        let client = QmpClient::new(QmpConfig::new(config.qmp_socket.clone()));

        Ok(Self {
            config,
            client,
            // Geometry is unknown until the first screendump arrives.
            format: FrameFormat::rgb24(0, 0),
            interval,
            origin: Instant::now(),
            stopped: false,
            last_sequence: None,
            last_pts: None,
        })
    }

    /// QMP control socket this source uses.
    pub fn socket_path(&self) -> &Path {
        &self.config.qmp_socket
    }

    /// Directory used for temporary PPM files.
    pub fn screendump_dir(&self) -> PathBuf {
        self.config.screendump_dir()
    }

    /// Frame interval derived from the configured fps ceiling.
    pub fn frame_interval(&self) -> Duration {
        self.interval
    }

    /// True after [`CaptureSource::stop`] has run.
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// True while the QMP control socket is connected.
    pub fn is_connected(&self) -> bool {
        self.client.control_connected()
    }

    /// Unique path for the next screendump, inside the configured directory.
    fn next_screendump_path(&self) -> PathBuf {
        let counter = SCREENDUMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.config
            .screendump_dir()
            .join(format!("ozma-qmp-{}-{counter}.ppm", std::process::id()))
    }
}

#[async_trait]
impl CaptureSource for QmpDisplaySource {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped {
            return Err(MediaError::EndOfStream);
        }

        if !self.client.control_connected() {
            self.client
                .connect_control()
                .await
                .map_err(|error| qmp_error(&self.config.id, "connect", error))?;
        }

        if let Some(directory) = &self.config.screendump_path {
            tokio::fs::create_dir_all(directory)
                .await
                .map_err(|error| directory_error(&self.config.id, directory, error))?;
        }

        let started = Instant::now();
        let path = self.next_screendump_path();

        if let Err(error) = self.client.screendump(&path).await {
            remove_screendump(&path).await;
            return Err(qmp_error(&self.config.id, "screendump", error));
        }

        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                remove_screendump(&path).await;
                bytes
            }
            Err(error) => {
                remove_screendump(&path).await;
                return Err(MediaError::capture(
                    &self.config.id,
                    format!(
                        "failed to read QMP screendump `{}`: {error}",
                        path.display()
                    ),
                ));
            }
        };

        let (width, height, payload) = parse_ppm_p6(&bytes).map_err(MediaError::from)?;

        if width > self.config.max_width || height > self.config.max_height {
            return Err(MediaError::from(
                ApiError::new(
                    codes::CAPTURE_UNAVAILABLE,
                    format!(
                        "QMP screendump is {}x{}, above the configured {}x{} limit.",
                        width, height, self.config.max_width, self.config.max_height
                    ),
                )
                .with_details(self.config.qmp_socket.display().to_string())
                .with_remediation(
                    "Raise the QMP display limits or configure a smaller guest resolution.",
                ),
            ));
        }

        let elapsed = started.elapsed();
        if elapsed < self.interval {
            tokio::time::sleep(self.interval - elapsed).await;
        }

        if self.stopped {
            return Err(MediaError::EndOfStream);
        }

        let pts = monotonic_pts(elapsed_micros(self.origin), self.last_pts);
        let sequence = next_sequence(self.last_sequence);
        let format = FrameFormat::rgb24(width, height);
        let frame = Frame::with_data(format, pts, sequence, false, payload)?;

        self.last_sequence = Some(sequence);
        self.last_pts = Some(pts);
        self.format = format;
        Ok(frame)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.client.disconnect().await;
        Ok(())
    }
}

/// Parse a binary PPM (`P6`) image into packed RGB24.
///
/// The header may contain `#` comments and arbitrary ASCII whitespace between
/// tokens. Exactly one whitespace byte separates the maximum value from the
/// raster (a CRLF pair counts as one separator). The payload must be exactly
/// `width * height * 3` bytes.
///
/// Rejects non-P6 files (including `P3` and `P5`), maximum values other than
/// 255 (so 16-bit images are rejected), zero or oversized dimensions, a
/// truncated header or payload, and trailing bytes. Every failure is a typed
/// [`ApiError`]; the parser never panics on arbitrary input.
pub fn parse_ppm_p6(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), ApiError> {
    let mut cursor = 0usize;

    let magic =
        next_header_token(bytes, &mut cursor).ok_or_else(|| ppm_error("the header is empty"))?;
    if magic != b"P6" {
        return Err(ppm_error(format!(
            "magic is `{}`; only binary P6 (RGB24) is supported",
            String::from_utf8_lossy(magic)
        )));
    }

    let width = parse_dimension(bytes, &mut cursor, "width")?;
    let height = parse_dimension(bytes, &mut cursor, "height")?;
    let maxval = parse_number(bytes, &mut cursor, "maximum value")?;
    if maxval != 255 {
        return Err(ppm_error(format!(
            "maximum value is {maxval}; only 8-bit P6 (255) is supported"
        )));
    }

    let separator = *bytes
        .get(cursor)
        .ok_or_else(|| ppm_error("the header ends before the raster"))?;
    if !separator.is_ascii_whitespace() {
        return Err(ppm_error("the maximum value is not followed by whitespace"));
    }
    cursor += 1;
    if separator == b'\r' && bytes.get(cursor) == Some(&b'\n') {
        cursor += 1;
    }

    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| ppm_error("dimensions overflow the pixel count"))?;
    let expected =
        usize::try_from(expected).map_err(|_| ppm_error("frame is too large for this platform"))?;

    let available = bytes.len() - cursor;
    if available < expected {
        return Err(ppm_error(format!(
            "raster is {available} bytes; {width}x{height} RGB24 needs {expected}"
        )));
    }
    if available > expected {
        return Err(ppm_error(format!(
            "raster has {} trailing bytes after the {expected}-byte frame",
            available - expected
        )));
    }

    Ok((width, height, bytes[cursor..].to_vec()))
}

fn parse_dimension(bytes: &[u8], cursor: &mut usize, label: &str) -> Result<u32, ApiError> {
    let value = parse_number(bytes, cursor, label)?;
    if value == 0 {
        return Err(ppm_error(format!("{label} must be non-zero")));
    }
    if value > MAX_PPM_DIMENSION {
        return Err(ppm_error(format!(
            "{label} {value} exceeds the {MAX_PPM_DIMENSION}-pixel limit"
        )));
    }
    Ok(value)
}

fn parse_number(bytes: &[u8], cursor: &mut usize, label: &str) -> Result<u32, ApiError> {
    let token =
        next_header_token(bytes, cursor).ok_or_else(|| ppm_error(format!("missing {label}")))?;
    let text =
        std::str::from_utf8(token).map_err(|_| ppm_error(format!("{label} is not ASCII")))?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ppm_error(format!("{label} `{text}` is not a number")));
    }
    text.parse::<u32>()
        .map_err(|_| ppm_error(format!("{label} `{text}` is out of range")))
}

/// Return the next whitespace-delimited header token, skipping whitespace and
/// `#` comments.
fn next_header_token<'a>(bytes: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    loop {
        while *cursor < bytes.len() && bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
        if *cursor < bytes.len() && bytes[*cursor] == b'#' {
            while *cursor < bytes.len() && bytes[*cursor] != b'\n' {
                *cursor += 1;
            }
            continue;
        }
        break;
    }

    let start = *cursor;
    while *cursor < bytes.len() && !bytes[*cursor].is_ascii_whitespace() && bytes[*cursor] != b'#' {
        *cursor += 1;
    }
    if *cursor == start {
        None
    } else {
        Some(&bytes[start..*cursor])
    }
}

fn ppm_error(message: impl Into<String>) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!("Invalid PPM screendump: {}.", message.into()),
    )
    .with_remediation("Ensure QEMU screendump writes a binary P6 PPM (8-bit RGB) file.")
}

fn missing_socket_error(path: &Path) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!("QMP socket `{}` does not exist.", path.display()),
    )
    .with_details(path.display().to_string())
    .with_remediation("Start the VM with a QMP socket at the configured path, then retry.")
    .retryable()
}

fn directory_error(id: &str, path: &Path, error: std::io::Error) -> MediaError {
    MediaError::capture(
        id,
        format!(
            "failed to create QMP screendump directory `{}`: {error}",
            path.display()
        ),
    )
}

fn qmp_error(id: &str, stage: &str, error: QmpError) -> MediaError {
    MediaError::capture(id, format!("QMP {stage} failed: {error}"))
}

async fn remove_screendump(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                path = %path.display(),
                %error,
                "failed to remove QMP screendump file"
            );
        }
    }
}

fn elapsed_micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ppm(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut data = format!("P6\n{width} {height}\n255\n").into_bytes();
        data.extend_from_slice(pixels);
        data
    }

    #[test]
    fn parses_a_valid_2x2_frame() {
        let pixels = [
            1, 2, 3, 4, 5, 6, //
            7, 8, 9, 10, 11, 12,
        ];
        let data = ppm(2, 2, &pixels);
        let (width, height, payload) = parse_ppm_p6(&data).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!(payload, pixels);
    }

    #[test]
    fn accepts_comments_and_arbitrary_whitespace() {
        let pixels = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let data = b"# leading comment\nP6\t# geometry\n 2 # value\n2\n# max\n255\n".to_vec();
        let mut data = data;
        data.extend_from_slice(&pixels);
        let (width, height, payload) = parse_ppm_p6(&data).unwrap();
        assert_eq!((width, height), (2, 2));
        assert_eq!(payload, pixels);
    }

    #[test]
    fn parses_a_1x1_frame() {
        let data = ppm(1, 1, &[0, 127, 255]);
        let (width, height, payload) = parse_ppm_p6(&data).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(payload, [0, 127, 255]);
    }

    #[test]
    fn accepts_a_crlf_separator_before_the_raster() {
        let mut data = b"P6\r\n1 1\r\n255\r\n".to_vec();
        data.extend_from_slice(&[9, 8, 7]);
        let (_, _, payload) = parse_ppm_p6(&data).unwrap();
        assert_eq!(payload, [9, 8, 7]);
    }

    #[test]
    fn rejects_a_truncated_header() {
        let error = parse_ppm_p6(b"P6\n2 2").unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
    }

    #[test]
    fn rejects_a_truncated_payload() {
        let data = b"P6\n2 2\n255\n\x01\x02\x03".to_vec();
        let error = parse_ppm_p6(&data).unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("raster"));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut data = ppm(1, 1, &[1, 2, 3]);
        data.push(0xff);
        let error = parse_ppm_p6(&data).unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("trailing"));
    }

    #[test]
    fn rejects_p3_and_p5_and_other_magics() {
        for magic in [b"P3", b"P5", b"P4"] {
            let mut data = magic.to_vec();
            data.extend_from_slice(b"\n1 1\n255\n\x00\x00\x00");
            let error = parse_ppm_p6(&data).unwrap_err();
            assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
            assert!(error.message.contains("P6"), "{}", error.message);
        }
    }

    #[test]
    fn rejects_16_bit_maxval() {
        let mut data = b"P6\n1 1\n65535\n".to_vec();
        data.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        let error = parse_ppm_p6(&data).unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("65535"), "{}", error.message);
        assert!(error.message.contains("8-bit"), "{}", error.message);
    }

    #[test]
    fn rejects_zero_dimensions() {
        let error = parse_ppm_p6(b"P6\n0 1\n255\n").unwrap_err();
        assert!(error.message.contains("non-zero"));
    }

    #[test]
    fn rejects_oversized_dimensions() {
        let header = format!("P6\n{} 1\n255\n", MAX_PPM_DIMENSION + 1);
        let error = parse_ppm_p6(header.as_bytes()).unwrap_err();
        assert!(error.message.contains("limit"), "{}", error.message);
    }

    #[test]
    fn rejects_non_numeric_dimensions() {
        let error = parse_ppm_p6(b"P6\nwide 1\n255\n").unwrap_err();
        assert!(error.message.contains("number"), "{}", error.message);
    }
}
