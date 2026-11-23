// SPDX-License-Identifier: AGPL-3.0-only
//! VNC/RFB display capture through ffmpeg, as a transition path.
//!
//! QEMU can expose its emulated display as a VNC server. This provider runs
//! ffmpeg with the VNC demuxer, asks it to scale to the configured geometry,
//! and reads packed RGB24 frames from stdout:
//!
//! ```text
//! QEMU VNC :5901 -> ffmpeg -f vnc -> rawvideo rgb24 -> CaptureSource
//! ```
//!
//! Unlike [`crate::r#virtual`], this provider writes raw frames straight into
//! the media pipeline. It needs no `v4l2loopback` module, no root, and no
//! `/dev/video*` device.
//!
//! ## Transition path, not the destination
//!
//! This is deliberately the smallest real VNC path that HLS/WebRTC can
//! consume. Limits:
//!
//! * ffmpeg's VNC input supports the unauthenticated loopback case used by a
//!   local QEMU display. Password-authenticated VNC is not handled here.
//! * Every frame is scaled to the configured `width`x`height`; the guest's
//!   native resolution is not negotiated or tracked.
//! * VNC carries no audio and no input injection; those stay on other
//!   providers.
//! * The D-Bus framebuffer provider remains the lower-latency VM path in the
//!   reconciliation plan.
//!
//! ## Process lifecycle
//!
//! [`VncDisplayProvider::enumerate`] is side-effect-free: it validates the
//! configuration and reports one capability without opening a socket or
//! spawning ffmpeg. [`VncDisplayProvider::open`] still does not spawn.
//! [`VncDisplaySource`] spawns ffmpeg lazily on the first
//! [`CaptureSource::next_frame`] call, so a failed VNC connection cannot crash
//! node startup.
//!
//! ffmpeg writes exactly `width * height * 3` bytes per frame. A clean EOF is
//! [`MediaError::EndOfStream`]; a truncated frame is a typed capture error.
//! stderr is drained into a bounded tail (see [`MAX_STDERR_BYTES`]) so a noisy
//! ffmpeg cannot fill a pipe or grow memory without bound.
//!
//! ## Testability
//!
//! Process/read I/O goes through [`VncSpawner`]/[`VncChild`]. The default suite
//! injects a fake child backed by a cursor and never needs ffmpeg or a VNC
//! server. The real test is `#[ignore]`d and gated by `OZMA_VNC_TEST=1`.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::provider::CaptureProvider;
use crate::v4l2::{monotonic_pts, next_sequence};

/// Default source id (`vnc0`).
pub const VNC_DEFAULT_ID: &str = "vnc0";

/// Default human-readable source name.
pub const VNC_DEFAULT_NAME: &str = "QEMU VNC display";

/// Default VNC host, matching QEMU's loopback display.
pub const VNC_DEFAULT_HOST: &str = "127.0.0.1";

/// Default VNC port for the first VM display.
pub const VNC_DEFAULT_PORT: u16 = 5901;

/// Default advertised width.
pub const VNC_DEFAULT_WIDTH: u32 = 1920;

/// Default advertised height.
pub const VNC_DEFAULT_HEIGHT: u32 = 1080;

/// Default frame rate.
pub const VNC_DEFAULT_FPS: u32 = 30;

/// Default ffmpeg executable (resolved through `PATH`).
pub const VNC_DEFAULT_FFMPEG_BIN: &str = "ffmpeg";

/// Default bound on the wait for the first frame / ffmpeg startup.
pub const VNC_DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest width or height this provider accepts.
///
/// This is a sanity bound against absurd allocations and corrupt
/// configuration, not a product limit. It matches the QMP PPM parser's bound.
pub const MAX_VNC_DIMENSION: u32 = 16_384;

/// Upper bound on the retained ffmpeg stderr tail.
pub const MAX_STDERR_BYTES: usize = 4_096;

/// Configuration for [`VncDisplayProvider`] and [`VncDisplaySource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VncDisplayConfig {
    /// Stable capture source id.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Host of QEMU's VNC listener.
    pub host: String,
    /// Port of QEMU's VNC listener.
    pub port: u16,
    /// Output width requested from ffmpeg.
    pub width: u32,
    /// Output height requested from ffmpeg.
    pub height: u32,
    /// Output frame rate requested from ffmpeg (must be non-zero).
    pub fps: u32,
    /// ffmpeg executable (a bare `ffmpeg` uses `PATH`).
    pub ffmpeg_bin: PathBuf,
    /// Bound on spawning ffmpeg and receiving the first frame.
    pub connect_timeout: Duration,
}

impl Default for VncDisplayConfig {
    fn default() -> Self {
        Self::new(VNC_DEFAULT_HOST, VNC_DEFAULT_PORT)
    }
}

impl VncDisplayConfig {
    /// Configuration for a VNC endpoint, with the VM-profile defaults.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            id: VNC_DEFAULT_ID.to_owned(),
            name: VNC_DEFAULT_NAME.to_owned(),
            host: host.into(),
            port,
            width: VNC_DEFAULT_WIDTH,
            height: VNC_DEFAULT_HEIGHT,
            fps: VNC_DEFAULT_FPS,
            ffmpeg_bin: PathBuf::from(VNC_DEFAULT_FFMPEG_BIN),
            connect_timeout: VNC_DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.id = id.into();
        self.name = name.into();
        self
    }

    /// Override the scaled output geometry.
    pub fn with_dimensions(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Override the output frame rate.
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    /// Override the ffmpeg executable.
    pub fn with_ffmpeg_bin(mut self, bin: impl Into<PathBuf>) -> Self {
        self.ffmpeg_bin = bin.into();
        self
    }

    /// Override the startup/first-frame timeout.
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// `host:port` as accepted by ffmpeg's VNC input.
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Bytes in one packed RGB24 frame, or `None` when the geometry overflows.
    pub fn frame_len(&self) -> Option<usize> {
        u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|pixels| pixels.checked_mul(3))
            .and_then(|bytes| usize::try_from(bytes).ok())
    }

    /// Reject configuration that ffmpeg or the frame reader cannot honour.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.host.trim().is_empty() {
            return Err(ApiError::invalid(
                "The VNC display source requires a non-empty host.",
            ));
        }
        if self.port == 0 {
            return Err(ApiError::invalid(
                "The VNC display source requires a non-zero port.",
            ));
        }
        if self.id.trim().is_empty() {
            return Err(ApiError::invalid(
                "The VNC display source requires a non-empty id.",
            ));
        }
        if self.name.trim().is_empty() {
            return Err(ApiError::invalid(
                "The VNC display source requires a non-empty name.",
            ));
        }
        if self.width == 0 || self.height == 0 {
            return Err(ApiError::invalid(
                "VNC display dimensions must be non-zero.",
            ));
        }
        if self.width > MAX_VNC_DIMENSION || self.height > MAX_VNC_DIMENSION {
            return Err(ApiError::invalid(format!(
                "VNC display dimensions {}x{} exceed the {MAX_VNC_DIMENSION}-pixel bound.",
                self.width, self.height
            )));
        }
        if self.frame_len().is_none() {
            return Err(ApiError::invalid(
                "VNC display dimensions overflow the RGB24 frame size.",
            ));
        }
        if self.fps == 0 {
            return Err(ApiError::invalid(
                "The VNC display source requires a non-zero frame rate.",
            ));
        }
        if self.ffmpeg_bin.as_os_str().is_empty()
            || self.ffmpeg_bin.to_string_lossy().trim().is_empty()
        {
            return Err(ApiError::invalid(
                "The VNC display source requires a non-empty ffmpeg path.",
            ));
        }
        if self.connect_timeout.is_zero() {
            return Err(ApiError::invalid(
                "The VNC display connect timeout must be greater than zero.",
            ));
        }
        Ok(())
    }
}

/// Build the exact ffmpeg argument vector for one VNC display.
///
/// Chosen argv (verified against the local ffmpeg's option surface):
///
/// ```text
/// ffmpeg -hide_banner -loglevel error -f vnc -i <host>:<port>
///        -vf scale=<width>:<height> -pix_fmt rgb24
///        -f rawvideo -r <fps> -
/// ```
///
/// `-vf scale=W:H` is used instead of the older `-s WxH` output-size option:
/// both exist in current ffmpeg, but `scale` is the explicit filter form, is
/// the form the Python `VirtualCapture` already uses, and keeps the intent
/// (scale the decoded VNC surface) unambiguous. `-f rawvideo` plus `-` makes
/// ffmpeg write the frames to stdout.
///
/// Note: a distro ffmpeg must be built with the VNC demuxer (libvncclient or
/// the native client). This checkout's host ffmpeg reports
/// `Unknown format 'vnc'`, which is why the live test is opt-in and the
/// default suite injects a fake child.
pub fn ffmpeg_argv(config: &VncDisplayConfig) -> Vec<String> {
    vec![
        config.ffmpeg_bin.to_string_lossy().into_owned(),
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-f".to_owned(),
        "vnc".to_owned(),
        "-i".to_owned(),
        config.endpoint(),
        "-vf".to_owned(),
        format!("scale={}:{}", config.width, config.height),
        "-pix_fmt".to_owned(),
        PixelFormat::Rgb24.as_str().to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-r".to_owned(),
        config.fps.to_string(),
        "-".to_owned(),
    ]
}

/// A spawnable ffmpeg child whose stdout is the raw-frame stream.
///
/// The trait exists so default tests can inject a cursor-backed fake instead
/// of starting ffmpeg. `wait` returns the exit code (`None` when the process
/// was terminated by a signal).
#[async_trait]
pub trait VncChild: Send {
    /// Read up to `buffer.len()` bytes. `Ok(0)` is end of stream.
    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize>;

    /// Terminate the child. Best effort; `wait` still reaps it.
    async fn kill(&mut self) -> io::Result<()>;

    /// Reap the child and return its exit code.
    async fn wait(&mut self) -> io::Result<Option<i32>>;

    /// Bounded tail of stderr, if the backend retains one.
    fn stderr_tail(&self) -> Option<String> {
        None
    }
}

/// Injectable process factory for [`VncDisplaySource`].
#[async_trait]
pub trait VncSpawner: Send + Sync {
    /// Spawn ffmpeg with `argv` and return its raw-frame child.
    async fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn VncChild>>;
}

/// Production [`VncSpawner`] backed by [`tokio::process`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessVncSpawner;

impl ProcessVncSpawner {
    /// A stateless spawner.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl VncSpawner for ProcessVncSpawner {
    async fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn VncChild>> {
        let (program, args) = argv.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "empty ffmpeg command argv")
        })?;

        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "ffmpeg did not expose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "ffmpeg did not expose stderr"))?;
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_buffer)));

        Ok(Box::new(ProcessVncChild {
            child,
            stdout,
            stderr_buffer,
            stderr_task: Some(stderr_task),
        }))
    }
}

/// The real ffmpeg child.
struct ProcessVncChild {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl VncChild for ProcessVncChild {
    async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buffer).await
    }

    async fn kill(&mut self) -> io::Result<()> {
        match self.child.kill().await {
            Ok(()) => Ok(()),
            // The child may have exited between the last read and stop.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn wait(&mut self) -> io::Result<Option<i32>> {
        let status = self.child.wait().await?;
        if let Some(mut task) = self.stderr_task.take() {
            // The pipe closes when the child exits; a grandchild that inherited
            // stderr must not be able to hold shutdown.
            if tokio::time::timeout(Duration::from_millis(250), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
        Ok(status.code())
    }

    fn stderr_tail(&self) -> Option<String> {
        let bytes = self
            .stderr_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if bytes.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&bytes).trim().to_owned())
        }
    }
}

/// Drain ffmpeg stderr into a bounded tail so the pipe never blocks.
async fn drain_stderr(mut stderr: tokio::process::ChildStderr, buffer: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 512];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let mut tail = buffer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                tail.extend_from_slice(&chunk[..read]);
                let excess = tail.len().saturating_sub(MAX_STDERR_BYTES);
                if excess > 0 {
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// A capture provider that reads QEMU's VNC display through ffmpeg.
#[derive(Clone, Debug)]
pub struct VncDisplayProvider {
    config: VncDisplayConfig,
}

impl VncDisplayProvider {
    /// Create a provider for `config`.
    pub fn new(config: VncDisplayConfig) -> Self {
        Self { config }
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &VncDisplayConfig {
        &self.config
    }

    /// The capability advertised for a valid configuration.
    ///
    /// This never connects to the VNC endpoint or starts ffmpeg.
    pub fn capability(&self) -> Option<CaptureCapability> {
        if self.config.validate().is_err() {
            return None;
        }

        Some(CaptureCapability {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            kind: CaptureKind::Vnc,
            formats: vec![PixelFormat::Rgb24.as_str().to_owned()],
            max_width: self.config.width,
            max_height: self.config.height,
            max_fps: self.config.fps,
            compressed: false,
            hdr: false,
            has_audio: false,
        })
    }
}

impl CaptureProvider for VncDisplayProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::Vnc
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        self.config.validate()?;
        Ok(self.capability().into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::Vnc || capability.id != self.config.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("VNC display source `{}` is not available.", capability.id),
            )
            .with_details(capability.id.clone()));
        }

        self.config.validate()?;
        Ok(Box::new(VncDisplaySource::new(self.config.clone())?))
    }
}

/// A [`CaptureSource`] that reads raw RGB24 frames from ffmpeg's VNC input.
pub struct VncDisplaySource {
    config: VncDisplayConfig,
    spawner: Arc<dyn VncSpawner>,
    child: Option<Box<dyn VncChild>>,
    format: FrameFormat,
    origin: Instant,
    stopped: bool,
    eof: bool,
    last_sequence: Option<u64>,
    last_pts: Option<u64>,
    last_error: Option<String>,
}

impl std::fmt::Debug for VncDisplaySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VncDisplaySource")
            .field("config", &self.config)
            .field("spawned", &self.child.is_some())
            .field("stopped", &self.stopped)
            .field("eof", &self.eof)
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl VncDisplaySource {
    /// Build a source using the production ffmpeg spawner.
    ///
    /// This validates configuration but does not spawn ffmpeg.
    pub fn new(config: VncDisplayConfig) -> Result<Self, ApiError> {
        Self::with_spawner(config, Arc::new(ProcessVncSpawner::new()))
    }

    /// Build a source with an injected process factory, for tests/embedding.
    pub fn with_spawner(
        config: VncDisplayConfig,
        spawner: Arc<dyn VncSpawner>,
    ) -> Result<Self, ApiError> {
        config.validate()?;
        let format = FrameFormat::rgb24(config.width, config.height);
        Ok(Self {
            config,
            spawner,
            child: None,
            format,
            origin: Instant::now(),
            stopped: false,
            eof: false,
            last_sequence: None,
            last_pts: None,
            last_error: None,
        })
    }

    /// `host:port` of the VNC endpoint.
    pub fn endpoint(&self) -> String {
        self.config.endpoint()
    }

    /// Frame interval derived from the configured fps.
    pub fn frame_interval(&self) -> Duration {
        Duration::from_micros((1_000_000 / u64::from(self.config.fps.max(1))).max(1))
    }

    /// True after [`CaptureSource::stop`] or a clean end of stream.
    pub fn is_stopped(&self) -> bool {
        self.stopped || self.eof
    }

    /// Last bounded ffmpeg error/status observed by this source.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Kill and reap the child, if any. Always leaves `child` empty.
    async fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            if let Err(error) = child.kill().await {
                tracing::warn!(
                    id = %self.config.id,
                    %error,
                    "failed to kill VNC ffmpeg child"
                );
            }
            if let Err(error) = child.wait().await {
                tracing::warn!(
                    id = %self.config.id,
                    %error,
                    "failed to reap VNC ffmpeg child"
                );
            }
        }
    }
}

#[async_trait]
impl CaptureSource for VncDisplaySource {
    fn id(&self) -> &str {
        &self.config.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped || self.eof {
            return Err(MediaError::EndOfStream);
        }

        let length = self.config.frame_len().ok_or_else(|| {
            MediaError::capture(
                &self.config.id,
                format!(
                    "VNC geometry {}x{} overflows the RGB24 frame size",
                    self.config.width, self.config.height
                ),
            )
        })?;

        let first_frame = self.child.is_none();
        if first_frame {
            match self.spawner.spawn(&ffmpeg_argv(&self.config)).await {
                Ok(child) => self.child = Some(child),
                Err(error) => {
                    let message = format!(
                        "could not start ffmpeg `{}` for VNC `{}`: {error}",
                        self.config.ffmpeg_bin.display(),
                        self.config.endpoint()
                    );
                    self.last_error = Some(message.clone());
                    return Err(MediaError::capture(&self.config.id, message));
                }
            }
        }

        let id = self.config.id.clone();
        let timeout = self.config.connect_timeout;
        let outcome = {
            let child = self
                .child
                .as_mut()
                .expect("VNC ffmpeg child was just spawned");
            let read = read_frame_bytes(child.as_mut(), length, &id);
            if first_frame {
                match tokio::time::timeout(timeout, read).await {
                    Ok(result) => result,
                    Err(_) => Err(MediaError::capture(
                        &id,
                        format!(
                            "timed out after {:?} waiting for the first VNC frame from `{}`",
                            timeout,
                            self.config.endpoint()
                        ),
                    )),
                }
            } else {
                read.await
            }
        };

        let payload = match outcome {
            Ok(Some(payload)) => payload,
            Ok(None) => {
                // Clean EOF: reap the child and make later calls idempotent.
                self.eof = true;
                let tail = self.child.as_ref().and_then(|child| child.stderr_tail());
                if tail.is_some() {
                    self.last_error = tail;
                }
                self.kill_child().await;
                return Err(MediaError::EndOfStream);
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                // The raw stream may be misaligned after a partial frame or I/O
                // failure; never try to resynchronise a byte stream.
                self.kill_child().await;
                return Err(error);
            }
        };

        let pts = monotonic_pts(elapsed_micros(self.origin), self.last_pts);
        let sequence = next_sequence(self.last_sequence);
        let format = FrameFormat::rgb24(self.config.width, self.config.height);
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
        self.kill_child().await;
        Ok(())
    }
}

/// Read exactly one frame, distinguishing clean EOF from a truncated frame.
async fn read_frame_bytes(
    child: &mut dyn VncChild,
    length: usize,
    id: &str,
) -> Result<Option<Vec<u8>>, MediaError> {
    let mut data = vec![0u8; length];
    let mut filled = 0usize;
    while filled < length {
        match child.read(&mut data[filled..]).await {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(MediaError::capture(
                    id,
                    format!(
                        "ffmpeg ended the VNC stream after {filled} of {length} RGB24 bytes; \
                         the configured geometry does not match the encoder output"
                    ),
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(MediaError::capture(
                    id,
                    format!("reading a VNC frame from ffmpeg failed: {error}"),
                ));
            }
        }
    }
    Ok(Some(data))
}

fn elapsed_micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    enum FakeMode {
        Data(Vec<u8>),
        Fail(io::ErrorKind),
        Block,
    }

    struct FakeSpawner {
        mode: FakeMode,
        spawned: Arc<AtomicUsize>,
        killed: Arc<AtomicUsize>,
        last_argv: Arc<Mutex<Vec<String>>>,
    }

    impl FakeSpawner {
        fn new(mode: FakeMode) -> (Arc<Self>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let spawned = Arc::new(AtomicUsize::new(0));
            let killed = Arc::new(AtomicUsize::new(0));
            (
                Arc::new(Self {
                    mode,
                    spawned: Arc::clone(&spawned),
                    killed: Arc::clone(&killed),
                    last_argv: Arc::new(Mutex::new(Vec::new())),
                }),
                spawned,
                killed,
            )
        }
    }

    #[async_trait]
    impl VncSpawner for FakeSpawner {
        async fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn VncChild>> {
            self.spawned.fetch_add(1, Ordering::Relaxed);
            *self
                .last_argv
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = argv.to_vec();
            match &self.mode {
                FakeMode::Data(data) => Ok(Box::new(FakeChild {
                    cursor: Cursor::new(data.clone()),
                    killed: Arc::clone(&self.killed),
                })),
                FakeMode::Fail(kind) => Err(io::Error::new(*kind, "fake ffmpeg spawn failure")),
                FakeMode::Block => Ok(Box::new(BlockingChild {
                    killed: Arc::clone(&self.killed),
                })),
            }
        }
    }

    struct FakeChild {
        cursor: Cursor<Vec<u8>>,
        killed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl VncChild for FakeChild {
        async fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            std::io::Read::read(&mut self.cursor, buffer)
        }

        async fn kill(&mut self) -> io::Result<()> {
            self.killed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn wait(&mut self) -> io::Result<Option<i32>> {
            Ok(Some(0))
        }

        fn stderr_tail(&self) -> Option<String> {
            Some("fake ffmpeg stderr".to_owned())
        }
    }

    struct BlockingChild {
        killed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl VncChild for BlockingChild {
        async fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            std::future::pending::<()>().await;
            unreachable!("pending never resolves")
        }

        async fn kill(&mut self) -> io::Result<()> {
            self.killed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn wait(&mut self) -> io::Result<Option<i32>> {
            Ok(None)
        }
    }

    fn small_config() -> VncDisplayConfig {
        VncDisplayConfig::new("127.0.0.1", 5901)
            .with_dimensions(2, 2)
            .with_fps(5)
    }

    fn frame_bytes(marker: u8) -> Vec<u8> {
        vec![marker; 2 * 2 * 3]
    }

    fn test_source(
        config: VncDisplayConfig,
        mode: FakeMode,
    ) -> (VncDisplaySource, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (spawner, spawned, killed) = FakeSpawner::new(mode);
        (
            VncDisplaySource::with_spawner(config, spawner).unwrap(),
            spawned,
            killed,
        )
    }

    #[test]
    fn defaults_match_the_vm_profile() {
        let config = VncDisplayConfig::default();
        assert_eq!(config.id, "vnc0");
        assert_eq!(config.name, "QEMU VNC display");
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 5901);
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(config.fps, 30);
        assert_eq!(config.ffmpeg_bin, PathBuf::from("ffmpeg"));
        assert_eq!(config.endpoint(), "127.0.0.1:5901");
    }

    #[test]
    fn argv_uses_vnc_input_scaling_and_rawvideo_stdout() {
        let config = small_config().with_ffmpeg_bin("/usr/bin/ffmpeg");
        assert_eq!(
            ffmpeg_argv(&config),
            vec![
                "/usr/bin/ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "vnc",
                "-i",
                "127.0.0.1:5901",
                "-vf",
                "scale=2:2",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "-r",
                "5",
                "-",
            ]
        );
    }

    #[test]
    fn validation_rejects_empty_and_zero_fields() {
        let mut config = VncDisplayConfig::default();
        config.host = "  ".into();
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.id.clear();
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.name.clear();
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.port = 0;
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.width = 0;
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.height = MAX_VNC_DIMENSION + 1;
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.fps = 0;
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.ffmpeg_bin = PathBuf::new();
        assert!(config.validate().is_err());

        let mut config = VncDisplayConfig::default();
        config.connect_timeout = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn enumerate_reports_one_rgb24_capability_without_connecting() {
        // `.invalid` is reserved and never resolves; enumeration must not
        // touch it, spawn ffmpeg, or block.
        let config = VncDisplayConfig::new("no-such-host.invalid", 5901)
            .with_dimensions(1280, 720)
            .with_fps(15);
        let provider = VncDisplayProvider::new(config.clone());

        assert_eq!(provider.kind(), CaptureKind::Vnc);
        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        let capability = &capabilities[0];
        assert_eq!(capability.id, "vnc0");
        assert_eq!(capability.kind, CaptureKind::Vnc);
        assert_eq!(capability.formats, vec!["rgb24".to_owned()]);
        assert_eq!((capability.max_width, capability.max_height), (1280, 720));
        assert_eq!(capability.max_fps, 15);
        assert!(!capability.compressed);
        assert!(!capability.hdr);
        assert!(!capability.has_audio);
        assert_eq!(capability.name, config.name);
    }

    #[test]
    fn enumerate_rejects_an_invalid_config() {
        let mut config = VncDisplayConfig::default();
        config.port = 0;
        let provider = VncDisplayProvider::new(config);
        let error = provider.enumerate().unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn open_rejects_unknown_id_and_kind() {
        let provider = VncDisplayProvider::new(small_config());
        let mut capability = provider.capability().unwrap();

        capability.id = "other".into();
        assert_eq!(
            provider.open(&capability).err().unwrap().code,
            codes::NOT_FOUND
        );

        capability.id = "vnc0".into();
        capability.kind = CaptureKind::Qmp;
        assert_eq!(
            provider.open(&capability).err().unwrap().code,
            codes::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn fake_child_reads_two_frames_then_end_of_stream() {
        let mut data = frame_bytes(1);
        data.extend(frame_bytes(2));
        let (mut source, spawned, _) = test_source(small_config(), FakeMode::Data(data));

        let first = source.next_frame().await.unwrap();
        assert_eq!(first.format(), FrameFormat::rgb24(2, 2));
        assert_eq!(first.data(), frame_bytes(1).as_slice());
        assert_eq!(first.sequence(), 0);

        let second = source.next_frame().await.unwrap();
        assert_eq!(second.data(), frame_bytes(2).as_slice());
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() >= first.pts_micros());

        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
        assert_eq!(spawned.load(Ordering::Relaxed), 1);
        assert!(source.is_stopped());
        // EOF retained the bounded stderr tail as the last status.
        assert_eq!(source.last_error(), Some("fake ffmpeg stderr"));
    }

    #[tokio::test]
    async fn short_frame_is_a_typed_capture_error() {
        let mut data = frame_bytes(1);
        data.truncate(data.len() - 1);
        let (mut source, _, killed) = test_source(small_config(), FakeMode::Data(data));

        let error = source.next_frame().await.unwrap_err();
        match error {
            MediaError::Capture { id, message } => {
                assert_eq!(id, "vnc0");
                assert!(message.contains("11 of 12"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert_eq!(killed.load(Ordering::Relaxed), 1);
        assert!(source.last_error().is_some());
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_later_reads_are_end_of_stream() {
        let (mut source, spawned, killed) = test_source(small_config(), FakeMode::Data(Vec::new()));

        // Never spawned: stopping is still a clean no-op.
        source.stop().await.unwrap();
        source.stop().await.unwrap();
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
        assert_eq!(spawned.load(Ordering::Relaxed), 0);
        assert_eq!(killed.load(Ordering::Relaxed), 0);

        let (mut source, spawned, killed) =
            test_source(small_config(), FakeMode::Data(frame_bytes(3)));
        source.next_frame().await.unwrap();
        source.stop().await.unwrap();
        source.stop().await.unwrap();
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
        assert_eq!(spawned.load(Ordering::Relaxed), 1);
        assert_eq!(killed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn spawn_failure_is_a_typed_capture_error() {
        let (mut source, _, _) = test_source(
            small_config().with_ffmpeg_bin("/definitely/missing/ffmpeg"),
            FakeMode::Fail(io::ErrorKind::NotFound),
        );

        let error = source.next_frame().await.unwrap_err();
        match error {
            MediaError::Capture { message, .. } => {
                assert!(message.contains("could not start ffmpeg"), "{message}");
                assert!(message.contains("127.0.0.1:5901"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert!(source.last_error().is_some());
    }

    #[tokio::test]
    async fn first_frame_timeout_kills_the_child() {
        let config = small_config().with_connect_timeout(Duration::from_millis(10));
        let (mut source, _, killed) = test_source(config, FakeMode::Block);

        let error = source.next_frame().await.unwrap_err();
        match error {
            MediaError::Capture { message, .. } => {
                assert!(message.contains("timed out"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert_eq!(killed.load(Ordering::Relaxed), 1);
        assert!(source.last_error().is_some());
    }

    #[tokio::test]
    async fn missing_ffmpeg_binary_is_a_typed_start_error() {
        // Exercise the production spawner without ffmpeg or a VNC server: the
        // executable is absent, so the failure happens before any read.
        let config = small_config().with_ffmpeg_bin("/definitely/missing/ozma-ffmpeg");
        let mut source = VncDisplaySource::new(config).unwrap();

        let error = source.next_frame().await.unwrap_err();
        match error {
            MediaError::Capture { message, .. } => {
                assert!(message.contains("could not start ffmpeg"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert!(source.last_error().is_some());
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg with VNC input and a live VNC server; set OZMA_VNC_TEST=1"]
    async fn real_vnc_ffmpeg_captures_a_frame() {
        if std::env::var("OZMA_VNC_TEST").as_deref() != Ok("1") {
            eprintln!("skipping: set OZMA_VNC_TEST=1 and expose a VNC server");
            return;
        }
        let host = std::env::var("OZMA_VNC_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
        let port = std::env::var("OZMA_VNC_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5901);
        let config = VncDisplayConfig::new(host, port)
            .with_dimensions(640, 480)
            .with_fps(5)
            .with_connect_timeout(Duration::from_secs(10));
        let provider = VncDisplayProvider::new(config);

        let capabilities = provider.enumerate().unwrap();
        let mut source = provider.open(&capabilities[0]).unwrap();
        let frame = source.next_frame().await.unwrap();
        assert_eq!(frame.format(), FrameFormat::rgb24(640, 480));
        assert_eq!(frame.len(), 640 * 480 * 3);
        source.stop().await.unwrap();
    }
}
