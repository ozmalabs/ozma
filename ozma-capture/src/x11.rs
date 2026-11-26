// SPDX-License-Identifier: AGPL-3.0-only
//! X11 screen capture through ffmpeg's `x11grab` input.
//!
//! This is the fallback for a desktop session that has a real X11 display but
//! no XDG Desktop Portal ScreenCast flow (or whose compositor exposes the
//! screen only through X11, not PipeWire):
//!
//! ```text
//! X11 display -> ffmpeg -f x11grab -> rawvideo rgb24 -> CaptureSource
//! ```
//!
//! ffmpeg grabs the configured X11 display, optionally offset by `+x,y`, and
//! writes packed RGB24 frames to stdout. The provider reads exactly
//! `width * height * 3` bytes per frame, paces frames to the configured rate,
//! and returns typed errors for a missing display, a spawn failure, a
//! truncated frame, or a clean end of stream.
//!
//! ## Configuration, not probing
//!
//! An X11 display is profile configuration, never something a hardware/VM node
//! should discover implicitly. [`X11CaptureProvider::enumerate`] validates the
//! configured display string and advertises one capability; it does not open an
//! X connection, run `xdpyinfo`, or spawn ffmpeg. An empty display is invalid
//! configuration, and a display that does not exist surfaces at open/first-frame
//! time as a typed capture error instead of silently selecting some other
//! screen.
//!
//! ## Testability
//!
//! Process/read I/O goes through [`X11Spawner`]/[`X11Child`]. The default suite
//! injects a cursor-backed fake child, so it needs neither ffmpeg nor an X
//! server. The real test is `#[ignore]`d and gated by `OZMA_X11_TEST=1`.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::provider::{monotonic_pts, next_sequence, CaptureProvider};

/// Default source id (`x11`).
pub const X11_DEFAULT_ID: &str = "x11";

/// Default human-readable source name.
pub const X11_DEFAULT_NAME: &str = "X11 display";

/// Default ffmpeg executable (resolved through `PATH`).
pub const X11_DEFAULT_FFMPEG_BIN: &str = "ffmpeg";

/// Default advertised width.
pub const X11_DEFAULT_WIDTH: u32 = 1920;

/// Default advertised height.
pub const X11_DEFAULT_HEIGHT: u32 = 1080;

/// Default frame rate.
pub const X11_DEFAULT_FPS: u32 = 30;

/// Default bound on spawning ffmpeg and receiving the first frame.
pub const X11_DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest width or height this provider accepts.
///
/// This is an allocation sanity bound, not a product limit.
pub const MAX_X11_DIMENSION: u32 = 16_384;

/// Largest frame rate this provider accepts.
pub const MAX_X11_FPS: u32 = 240;

/// Upper bound on the retained ffmpeg stderr tail.
pub const X11_MAX_STDERR_BYTES: usize = 4_096;

/// Configuration for [`X11CaptureProvider`] and [`X11CaptureSource`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11CaptureConfig {
    /// Stable capture source id.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// X11 display (`:0`, `:0.0`, or `host:10.0`). Required.
    pub display: String,
    /// X offset inside the display.
    pub x: u32,
    /// Y offset inside the display.
    pub y: u32,
    /// Output width requested from ffmpeg.
    pub width: u32,
    /// Output height requested from ffmpeg.
    pub height: u32,
    /// Output frame rate (non-zero).
    pub fps: u32,
    /// ffmpeg executable (a bare `ffmpeg` uses `PATH`).
    pub ffmpeg_bin: PathBuf,
    /// Bound on spawning ffmpeg and receiving the first frame.
    pub connect_timeout: Duration,
}

impl Default for X11CaptureConfig {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl X11CaptureConfig {
    /// Configuration for one X11 display, with the desktop defaults.
    ///
    /// An empty display is deliberately invalid: callers must name the display
    /// they intend to capture instead of inheriting whatever `DISPLAY` happens
    /// to be set in the node's environment.
    pub fn new(display: impl Into<String>) -> Self {
        Self {
            id: X11_DEFAULT_ID.to_owned(),
            name: X11_DEFAULT_NAME.to_owned(),
            display: display.into(),
            x: 0,
            y: 0,
            width: X11_DEFAULT_WIDTH,
            height: X11_DEFAULT_HEIGHT,
            fps: X11_DEFAULT_FPS,
            ffmpeg_bin: PathBuf::from(X11_DEFAULT_FFMPEG_BIN),
            connect_timeout: X11_DEFAULT_CONNECT_TIMEOUT,
        }
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.id = id.into();
        self.name = name.into();
        self
    }

    /// Offset the grab inside the display.
    pub fn with_offset(mut self, x: u32, y: u32) -> Self {
        self.x = x;
        self.y = y;
        self
    }

    /// Override the requested geometry.
    pub fn with_dimensions(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// Override the frame rate.
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

    /// ffmpeg's x11grab input argument: `<display>+x,y`.
    pub fn input(&self) -> String {
        format!("{}+{},{}", self.display, self.x, self.y)
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
        if self.display.is_empty() {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                "The X11 capture source requires an explicit display such as `:0`.",
            )
            .with_remediation(
                "Pass the X display (`:0`, `:0.0`, or `host:10.0`) in the node configuration.",
            ));
        }
        if self.display.trim() != self.display
            || self
                .display
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(ApiError::invalid(
                "The X11 display must not contain whitespace or control characters.",
            ));
        }
        if self.display.starts_with('-') {
            return Err(ApiError::invalid(
                "The X11 display must not start with `-` (it would be parsed as an ffmpeg option).",
            ));
        }
        if self.display.contains('+') {
            return Err(ApiError::invalid(
                "The X11 display must not include the `+x,y` region offset; use with_offset().",
            ));
        }
        if self.id.trim().is_empty() {
            return Err(ApiError::invalid(
                "The X11 capture source requires a non-empty id.",
            ));
        }
        if self.name.trim().is_empty() {
            return Err(ApiError::invalid(
                "The X11 capture source requires a non-empty name.",
            ));
        }
        if self.width == 0 || self.height == 0 {
            return Err(ApiError::invalid(
                "X11 capture dimensions must be non-zero.",
            ));
        }
        if self.width > MAX_X11_DIMENSION || self.height > MAX_X11_DIMENSION {
            return Err(ApiError::invalid(format!(
                "X11 capture dimensions {}x{} exceed the {MAX_X11_DIMENSION}-pixel bound.",
                self.width, self.height
            )));
        }
        if self.frame_len().is_none() {
            return Err(ApiError::invalid(
                "X11 capture dimensions overflow the RGB24 frame size.",
            ));
        }
        if self.fps == 0 {
            return Err(ApiError::invalid(
                "The X11 capture source requires a non-zero frame rate.",
            ));
        }
        if self.fps > MAX_X11_FPS {
            return Err(ApiError::invalid(format!(
                "The X11 capture source accepts at most {MAX_X11_FPS} fps (requested {}).",
                self.fps
            )));
        }
        if self.ffmpeg_bin.as_os_str().is_empty()
            || self.ffmpeg_bin.to_string_lossy().trim().is_empty()
        {
            return Err(ApiError::invalid(
                "The X11 capture source requires a non-empty ffmpeg path.",
            ));
        }
        if self.connect_timeout.is_zero() {
            return Err(ApiError::invalid(
                "The X11 capture connect timeout must be greater than zero.",
            ));
        }
        Ok(())
    }
}

/// Build the exact ffmpeg argument vector for one X11 capture.
///
/// ```text
/// ffmpeg -hide_banner -loglevel error -f x11grab -i <display>+x,y
///        -pix_fmt rgb24 -f rawvideo -s WxH -r fps -
/// ```
///
/// The `+x,y` suffix is part of x11grab's input grammar. `-s WxH` and `-r fps`
/// make ffmpeg scale/pace the output to exactly the geometry and rate this
/// provider reads, and `-` writes the raw frames to stdout. This mirrors the
/// requested desktop fallback shape; if a caller needs a true crop rather than
/// a scaled grab of the whole display, x11grab's input-side `-video_size` is the
/// next refinement (not used here so the output always matches `frame_len`).
pub fn x11grab_argv(config: &X11CaptureConfig) -> Vec<String> {
    vec![
        config.ffmpeg_bin.to_string_lossy().into_owned(),
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-f".to_owned(),
        "x11grab".to_owned(),
        "-i".to_owned(),
        config.input(),
        "-pix_fmt".to_owned(),
        PixelFormat::Rgb24.as_str().to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-s".to_owned(),
        format!("{}x{}", config.width, config.height),
        "-r".to_owned(),
        config.fps.to_string(),
        "-".to_owned(),
    ]
}

/// Alias matching the other ffmpeg-backed providers' argv helper name.
pub fn ffmpeg_argv(config: &X11CaptureConfig) -> Vec<String> {
    x11grab_argv(config)
}

/// A spawnable ffmpeg child whose stdout is the raw-frame stream.
#[async_trait]
pub trait X11Child: Send {
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

/// Injectable process factory for [`X11CaptureSource`].
#[async_trait]
pub trait X11Spawner: Send + Sync {
    /// Spawn ffmpeg with `argv` and return its raw-frame child.
    async fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn X11Child>>;
}

/// Production [`X11Spawner`] backed by [`tokio::process`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessX11Spawner;

impl ProcessX11Spawner {
    /// A stateless spawner.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl X11Spawner for ProcessX11Spawner {
    async fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn X11Child>> {
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
            .ok_or_else(|| io::Error::other("ffmpeg did not expose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg did not expose stderr"))?;
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_buffer)));

        Ok(Box::new(ProcessX11Child {
            child,
            stdout,
            stderr_buffer,
            stderr_task: Some(stderr_task),
        }))
    }
}

/// The real ffmpeg child.
struct ProcessX11Child {
    child: tokio::process::Child,
    stdout: tokio::process::ChildStdout,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait]
impl X11Child for ProcessX11Child {
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
                let excess = tail.len().saturating_sub(X11_MAX_STDERR_BYTES);
                if excess > 0 {
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// A capture provider that reads an X11 display through ffmpeg's x11grab.
#[derive(Clone, Debug)]
pub struct X11CaptureProvider {
    config: X11CaptureConfig,
}

impl X11CaptureProvider {
    /// Create a provider for `config`.
    pub fn new(config: X11CaptureConfig) -> Self {
        Self { config }
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &X11CaptureConfig {
        &self.config
    }

    /// The capability advertised for a valid configuration.
    ///
    /// This never opens an X connection or starts ffmpeg.
    pub fn capability(&self) -> Option<CaptureCapability> {
        if self.config.validate().is_err() {
            return None;
        }
        Some(CaptureCapability {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            kind: CaptureKind::X11,
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

impl CaptureProvider for X11CaptureProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::X11
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        self.config.validate()?;
        Ok(self.capability().into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::X11 || capability.id != self.config.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("X11 capture source `{}` is not available.", capability.id),
            )
            .with_details(capability.id.clone()));
        }

        self.config.validate()?;
        Ok(Box::new(X11CaptureSource::new(self.config.clone())?))
    }
}

/// A [`CaptureSource`] that reads raw RGB24 frames from ffmpeg's x11grab.
pub struct X11CaptureSource {
    config: X11CaptureConfig,
    spawner: Arc<dyn X11Spawner>,
    child: Option<Box<dyn X11Child>>,
    format: FrameFormat,
    origin: Instant,
    stopped: bool,
    eof: bool,
    last_sequence: Option<u64>,
    last_pts: Option<u64>,
    next_emit: Option<Instant>,
    last_error: Option<String>,
}

impl std::fmt::Debug for X11CaptureSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("X11CaptureSource")
            .field("config", &self.config)
            .field("spawned", &self.child.is_some())
            .field("stopped", &self.stopped)
            .field("eof", &self.eof)
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl X11CaptureSource {
    /// Build a source using the production ffmpeg spawner.
    ///
    /// This validates configuration but does not spawn ffmpeg.
    pub fn new(config: X11CaptureConfig) -> Result<Self, ApiError> {
        Self::with_spawner(config, Arc::new(ProcessX11Spawner::new()))
    }

    /// Build a source with an injected process factory, for tests/embedding.
    pub fn with_spawner(
        config: X11CaptureConfig,
        spawner: Arc<dyn X11Spawner>,
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
            next_emit: None,
            last_error: None,
        })
    }

    /// The ffmpeg x11grab input argument (`<display>+x,y`).
    pub fn input(&self) -> String {
        self.config.input()
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
                    "failed to kill X11 ffmpeg child"
                );
            }
            if let Err(error) = child.wait().await {
                tracing::warn!(
                    id = %self.config.id,
                    %error,
                    "failed to reap X11 ffmpeg child"
                );
            }
        }
    }
}

#[async_trait]
impl CaptureSource for X11CaptureSource {
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
                    "X11 geometry {}x{} overflows the RGB24 frame size",
                    self.config.width, self.config.height
                ),
            )
        })?;

        let first_frame = self.child.is_none();
        if first_frame {
            match self.spawner.spawn(&x11grab_argv(&self.config)).await {
                Ok(child) => self.child = Some(child),
                Err(error) => {
                    let message = format!(
                        "could not start ffmpeg `{}` for X11 display `{}`: {error}",
                        self.config.ffmpeg_bin.display(),
                        self.config.display
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
                .expect("X11 ffmpeg child was just spawned");
            let read = read_frame_bytes(child.as_mut(), length, &id);
            if first_frame {
                match tokio::time::timeout(timeout, read).await {
                    Ok(result) => result,
                    Err(_) => Err(MediaError::capture(
                        &id,
                        format!(
                            "timed out after {timeout:?} waiting for the first X11 frame from `{}`",
                            self.config.display
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

        // Pace delivery even if the child produced the frame early. A real
        // x11grab child is already paced by `-r`, so this usually adds no wait.
        if let Some(next_emit) = self.next_emit {
            let now = Instant::now();
            if now < next_emit {
                tokio::time::sleep(next_emit - now).await;
            }
        }
        self.next_emit = Some(Instant::now() + self.frame_interval());

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
    child: &mut dyn X11Child,
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
                        "ffmpeg ended the X11 stream after {filled} of {length} RGB24 bytes; \
                         the configured geometry does not match the x11grab output"
                    ),
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                return Err(MediaError::capture(
                    id,
                    format!("reading an X11 frame from ffmpeg failed: {error}"),
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
    impl X11Spawner for FakeSpawner {
        async fn spawn(&self, argv: &[String]) -> io::Result<Box<dyn X11Child>> {
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
                FakeMode::Fail(kind) => Err(io::Error::new(*kind, "fake x11grab spawn failure")),
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
    impl X11Child for FakeChild {
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
            Some("fake x11grab stderr".to_owned())
        }
    }

    struct BlockingChild {
        killed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl X11Child for BlockingChild {
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

    fn small_config() -> X11CaptureConfig {
        X11CaptureConfig::new(":0.0")
            .with_dimensions(2, 2)
            .with_fps(200)
    }

    fn frame_bytes(marker: u8) -> Vec<u8> {
        vec![marker; 2 * 2 * 3]
    }

    fn test_source(
        config: X11CaptureConfig,
        mode: FakeMode,
    ) -> (X11CaptureSource, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let (spawner, spawned, killed) = FakeSpawner::new(mode);
        (
            X11CaptureSource::with_spawner(config, spawner).unwrap(),
            spawned,
            killed,
        )
    }

    #[test]
    fn defaults_are_an_unconfigured_display() {
        let config = X11CaptureConfig::default();
        assert_eq!(config.id, "x11");
        assert_eq!(config.name, "X11 display");
        assert!(config.display.is_empty());
        assert_eq!((config.width, config.height), (1920, 1080));
        assert_eq!(config.fps, 30);
        assert_eq!(config.ffmpeg_bin, PathBuf::from("ffmpeg"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn argv_uses_x11grab_and_rawvideo_stdout() {
        let config = X11CaptureConfig::new(":0.0")
            .with_offset(10, 20)
            .with_dimensions(640, 480)
            .with_fps(15)
            .with_ffmpeg_bin("/usr/bin/ffmpeg");
        assert_eq!(
            x11grab_argv(&config),
            vec![
                "/usr/bin/ffmpeg",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "x11grab",
                "-i",
                ":0.0+10,20",
                "-pix_fmt",
                "rgb24",
                "-f",
                "rawvideo",
                "-s",
                "640x480",
                "-r",
                "15",
                "-",
            ]
        );
        assert_eq!(config.input(), ":0.0+10,20");
    }

    #[test]
    fn validation_rejects_missing_and_unsafe_displays() {
        let mut config = small_config();
        config.display.clear();
        assert_eq!(config.validate().unwrap_err().code, codes::INVALID_REQUEST);

        let mut config = small_config();
        config.display = ":0 +10,10".into();
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.display = "-i".into();
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.display = ":0.0+10,10".into();
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.width = 0;
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.height = MAX_X11_DIMENSION + 1;
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.fps = 0;
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.fps = MAX_X11_FPS + 1;
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.ffmpeg_bin = PathBuf::new();
        assert!(config.validate().is_err());

        let mut config = small_config();
        config.connect_timeout = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn enumerate_reports_one_capability_without_spawning() {
        let config = X11CaptureConfig::new(":99")
            .with_dimensions(1280, 720)
            .with_fps(20);
        let provider = X11CaptureProvider::new(config.clone());

        assert_eq!(provider.kind(), CaptureKind::X11);
        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        let capability = &capabilities[0];
        assert_eq!(capability.id, "x11");
        assert_eq!(capability.kind, CaptureKind::X11);
        assert_eq!(capability.formats, vec!["rgb24".to_owned()]);
        assert_eq!((capability.max_width, capability.max_height), (1280, 720));
        assert_eq!(capability.max_fps, 20);
        assert!(!capability.compressed);
        assert_eq!(capability.name, config.name);
    }

    #[test]
    fn enumerate_rejects_an_invalid_config() {
        let provider = X11CaptureProvider::new(X11CaptureConfig::default());
        assert_eq!(
            provider.enumerate().unwrap_err().code,
            codes::INVALID_REQUEST
        );
    }

    #[test]
    fn open_rejects_unknown_id_and_kind() {
        let provider = X11CaptureProvider::new(small_config());
        let mut capability = provider.capability().unwrap();

        capability.id = "other".into();
        assert_eq!(
            provider.open(&capability).err().unwrap().code,
            codes::NOT_FOUND
        );

        capability.id = "x11".into();
        capability.kind = CaptureKind::Vnc;
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
        assert_eq!(source.last_error(), Some("fake x11grab stderr"));
    }

    #[tokio::test]
    async fn short_frame_is_a_typed_capture_error() {
        let mut data = frame_bytes(1);
        data.truncate(data.len() - 1);
        let (mut source, _, killed) = test_source(small_config(), FakeMode::Data(data));

        match source.next_frame().await.unwrap_err() {
            MediaError::Capture { id, message } => {
                assert_eq!(id, "x11");
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

        match source.next_frame().await.unwrap_err() {
            MediaError::Capture { message, .. } => {
                assert!(message.contains("could not start ffmpeg"), "{message}");
                assert!(message.contains(":0.0"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert!(source.last_error().is_some());
    }

    #[tokio::test]
    async fn first_frame_timeout_kills_the_child() {
        let config = small_config().with_connect_timeout(Duration::from_millis(10));
        let (mut source, _, killed) = test_source(config, FakeMode::Block);

        match source.next_frame().await.unwrap_err() {
            MediaError::Capture { message, .. } => {
                assert!(message.contains("timed out"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert_eq!(killed.load(Ordering::Relaxed), 1);
        assert!(source.last_error().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn instant_frames_are_paced_at_the_configured_fps() {
        let config = small_config().with_fps(10);
        let data = frame_bytes(1).repeat(2);
        let (mut source, _, _) = test_source(config, FakeMode::Data(data));

        let start = tokio::time::Instant::now();
        source.next_frame().await.unwrap();
        source.next_frame().await.unwrap();
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "second frame should wait for the 100 ms frame interval, elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn missing_ffmpeg_binary_is_a_typed_start_error() {
        // Exercise the production spawner without ffmpeg or X11: the executable
        // is absent, so the failure happens before any read.
        let config = small_config().with_ffmpeg_bin("/definitely/missing/ozma-ffmpeg");
        let mut source = X11CaptureSource::new(config).unwrap();

        match source.next_frame().await.unwrap_err() {
            MediaError::Capture { message, .. } => {
                assert!(message.contains("could not start ffmpeg"), "{message}");
            }
            other => panic!("expected a capture error, got {other:?}"),
        }
        assert!(source.last_error().is_some());
    }

    #[tokio::test]
    #[ignore = "needs ffmpeg with x11grab and a live X11 display; set OZMA_X11_TEST=1"]
    async fn real_x11grab_captures_a_frame() {
        if std::env::var("OZMA_X11_TEST").as_deref() != Ok("1") {
            eprintln!("skipping: set OZMA_X11_TEST=1 and expose an X11 display");
            return;
        }
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_owned());
        let config = X11CaptureConfig::new(display)
            .with_dimensions(640, 480)
            .with_fps(5)
            .with_connect_timeout(Duration::from_secs(10));
        let provider = X11CaptureProvider::new(config);

        let capabilities = provider.enumerate().unwrap();
        let mut source = provider.open(&capabilities[0]).unwrap();
        let frame = source.next_frame().await.unwrap();
        assert_eq!(frame.format(), FrameFormat::rgb24(640, 480));
        assert_eq!(frame.data().len(), 640 * 480 * 3);
        source.stop().await.unwrap();
    }
}
