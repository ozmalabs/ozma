// SPDX-License-Identifier: AGPL-3.0-only
//! v4l2loopback-backed virtual capture for VNC/VM displays.
//!
//! A VM's emulated display can be published as an ordinary V4L2 device by
//! loading `v4l2loopback` and running ffmpeg with the VNC input:
//!
//! ```text
//! QEMU display -> VNC :5901 -> ffmpeg -> v4l2loopback -> /dev/videoN
//! ```
//!
//! Downstream capture code then treats the VM exactly like a USB capture card,
//! which is the behaviour the Python `VirtualCapture` provided.
//!
//! ## Failure is explicit
//!
//! `modprobe`, `v4l2loopback-ctl`, and `ffmpeg` are host packages. If any of
//! them is missing or fails, [`VirtualCapture::start`] returns a typed
//! [`ApiError`] with remediation. It never reports a device path that does not
//! back a running ffmpeg process. The compatibility note from the Python node
//! still applies: **the controller can stream VNC directly** when
//! v4l2loopback is unavailable, so a failed start is recoverable rather than
//! fatal.
//!
//! ## Privileges and testability
//!
//! Module operations need root; ffmpeg does not. The default privilege prefix
//! is `sudo`, matching the Python implementation, but it is injectable:
//! [`VirtualCapture::with_privilege_prefix`] and
//! [`VirtualCapture::without_privilege_prefix`] let a root service or a test
//! run without hard-coded `sudo`.
//!
//! All process creation goes through [`CommandRunner`]. Tests inject a scripted
//! runner and assert the exact argv and lifecycle without loading a kernel
//! module, touching `/dev/video*`, or starting ffmpeg.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;

use crate::provider::CaptureProvider;
use crate::v4l2::{parse_video_index, V4l2Capture, V4l2Node};

/// Default VNC host, matching the Python virtual node.
pub const DEFAULT_VNC_HOST: &str = "127.0.0.1";
/// Default VNC port for the first VM display.
pub const DEFAULT_VNC_PORT: u16 = 5901;
/// Default capture width.
pub const DEFAULT_WIDTH: u32 = 1024;
/// Default capture height.
pub const DEFAULT_HEIGHT: u32 = 768;
/// Default VNC polling/frame rate.
pub const DEFAULT_FPS: u32 = 20;
/// Default card label prefix; the full label is `"<prefix> <vm_name>"`.
pub const DEFAULT_CARD_LABEL_PREFIX: &str = "Ozma Virtual";
/// First device number considered when none is configured.
pub const DEFAULT_DEVICE_NUMBER_START: u16 = 10;
/// Upper bound applied to the ffmpeg `-r` argument.
pub const MAX_FPS: u32 = 240;

/// Configuration for one [`VirtualCapture`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualCaptureConfig {
    /// VM name, used in the card label and diagnostics.
    pub vm_name: String,
    /// Host of the VM's VNC endpoint.
    pub vnc_host: String,
    /// Port of the VM's VNC endpoint.
    pub vnc_port: u16,
    /// Requested output width.
    pub width: u32,
    /// Requested output height.
    pub height: u32,
    /// Requested frame rate; clamped to `1..=MAX_FPS` when building argv.
    pub fps: u32,
    /// Explicit `/dev/videoN` number, or `None` to allocate the next free one.
    pub device_number: Option<u16>,
    /// Prefix of the v4l2loopback card label.
    pub card_label_prefix: String,
}

impl Default for VirtualCaptureConfig {
    /// Every field at its Python-compatible default, with an empty VM name.
    fn default() -> Self {
        Self::new("")
    }
}

impl VirtualCaptureConfig {
    /// Defaults matching the Python `VirtualCapture` constructor.
    pub fn new(vm_name: impl Into<String>) -> Self {
        Self {
            vm_name: vm_name.into(),
            vnc_host: DEFAULT_VNC_HOST.to_owned(),
            vnc_port: DEFAULT_VNC_PORT,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fps: DEFAULT_FPS,
            device_number: None,
            card_label_prefix: DEFAULT_CARD_LABEL_PREFIX.to_owned(),
        }
    }

    /// Card label passed to v4l2loopback.
    pub fn card_label(&self) -> String {
        if self.card_label_prefix.is_empty() {
            self.vm_name.clone()
        } else if self.vm_name.is_empty() {
            self.card_label_prefix.clone()
        } else {
            format!("{} {}", self.card_label_prefix, self.vm_name)
        }
    }

    /// `host:port` as accepted by ffmpeg's VNC input.
    pub fn vnc_endpoint(&self) -> String {
        format!("{}:{}", self.vnc_host, self.vnc_port)
    }
}

/// Opaque handle for a child process owned by a [`CommandRunner`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChildId(pub u64);

/// Captured result of a short-lived command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status, or `None` when the process was terminated by a signal.
    pub code: Option<i32>,
    /// Standard output, lossily decoded as UTF-8.
    pub stdout: String,
    /// Standard error, lossily decoded as UTF-8.
    pub stderr: String,
}

impl CommandOutput {
    /// True when the process exited with status zero.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Injectable process boundary for the virtual-capture lifecycle.
///
/// The real implementation uses [`tokio::process::Command`]. Tests implement
/// this trait to assert argv and lifecycle without running host tools.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run a command to completion and capture its output.
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput>;

    /// Spawn a long-running child, such as ffmpeg.
    async fn spawn(&self, argv: &[String]) -> std::io::Result<ChildId>;

    /// Kill a child previously returned by [`CommandRunner::spawn`].
    async fn kill(&self, child: ChildId) -> std::io::Result<()>;
}

/// Production [`CommandRunner`] backed by tokio's process API.
#[derive(Debug, Default)]
pub struct ProcessCommandRunner {
    children: AsyncMutex<HashMap<u64, tokio::process::Child>>,
    next: AtomicU64,
}

impl ProcessCommandRunner {
    /// A runner with no children.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
        let (program, args) = split_argv(argv)?;
        let output = Command::new(program).args(args).output().await?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    async fn spawn(&self, argv: &[String]) -> std::io::Result<ChildId> {
        let (program, args) = split_argv(argv)?;
        let child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.children.lock().await.insert(id, child);
        Ok(ChildId(id))
    }

    async fn kill(&self, child: ChildId) -> std::io::Result<()> {
        let mut child = self.children.lock().await.remove(&child.0);
        match child.as_mut() {
            Some(child) => child.kill().await,
            None => Ok(()),
        }
    }
}

fn split_argv(argv: &[String]) -> std::io::Result<(&str, &[String])> {
    let Some(program) = argv.first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "empty command argv",
        ));
    };
    Ok((program, &argv[1..]))
}

type DeviceProbe = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

/// The v4l2loopback + ffmpeg lifecycle for one VM display.
pub struct VirtualCapture {
    config: VirtualCaptureConfig,
    runner: Arc<dyn CommandRunner>,
    privilege_prefix: Vec<String>,
    device_probe: DeviceProbe,
    child: Option<ChildId>,
    device_path: Option<PathBuf>,
    loopback_created: bool,
}

impl std::fmt::Debug for VirtualCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualCapture")
            .field("config", &self.config)
            .field("privilege_prefix", &self.privilege_prefix)
            .field("child", &self.child)
            .field("device_path", &self.device_path)
            .field("loopback_created", &self.loopback_created)
            .finish()
    }
}

impl VirtualCapture {
    /// Build a capture using the production process runner and `sudo` for
    /// module operations.
    ///
    /// Device creation is confirmed by waiting for `/dev/videoN` to appear, so
    /// a successful `modprobe` that created nothing is not reported as a
    /// working capture.
    pub fn new(config: VirtualCaptureConfig) -> Self {
        Self::with_runner(config, Arc::new(ProcessCommandRunner::new()))
            .with_device_probe(|path| path.exists())
    }

    /// Build a capture with an injected process runner.
    ///
    /// An injected runner is assumed to model device creation as well; the
    /// default probe therefore trusts it. Use
    /// [`VirtualCapture::with_device_probe`] when a test or a custom host
    /// wrapper needs a real filesystem check.
    pub fn with_runner(config: VirtualCaptureConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            config,
            runner,
            privilege_prefix: vec!["sudo".to_owned()],
            device_probe: Arc::new(|_| true),
            child: None,
            device_path: None,
            loopback_created: false,
        }
    }

    /// Replace the check used to confirm that a loopback device exists.
    pub fn with_device_probe<F>(mut self, probe: F) -> Self
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        self.device_probe = Arc::new(probe);
        self
    }

    /// Replace the privilege prefix (`sudo` by default).
    ///
    /// Module and `v4l2loopback-ctl` commands are prefixed; ffmpeg is not.
    pub fn with_privilege_prefix<I, S>(mut self, prefix: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.privilege_prefix = prefix.into_iter().map(Into::into).collect();
        self
    }

    /// Run module operations without a privilege wrapper (root services/tests).
    pub fn without_privilege_prefix(self) -> Self {
        self.with_privilege_prefix(Vec::<String>::new())
    }

    /// Configuration this capture was built with.
    pub fn config(&self) -> &VirtualCaptureConfig {
        &self.config
    }

    /// The `/dev/videoN` path, once [`VirtualCapture::start`] has succeeded.
    pub fn device_path(&self) -> Option<&Path> {
        self.device_path.as_deref()
    }

    /// True while an ffmpeg child is recorded as running.
    ///
    /// This reports lifecycle state, not a fresh `waitpid` check; a process
    /// that exited on its own is only observed by the caller's stream failing.
    pub fn active(&self) -> bool {
        self.child.is_some()
    }

    /// Command used to check whether the module is already loaded.
    pub fn module_probe_command(&self) -> Vec<String> {
        vec!["lsmod".to_owned()]
    }

    /// Command used to load the module without device parameters.
    pub fn ensure_module_command(&self) -> Vec<String> {
        self.privileged(&["modprobe", "v4l2loopback"])
    }

    /// Command that asks the module to create a labelled device.
    pub fn create_loopback_command(&self, device_number: u16) -> Vec<String> {
        let number = format!("video_nr={device_number}");
        let label = format!("card_label={}", self.config.card_label());
        self.privileged(&[
            "modprobe",
            "v4l2loopback",
            &number,
            &label,
            "exclusive_caps=1",
        ])
    }

    /// Fallback command used when the module is already loaded.
    pub fn create_loopback_ctl_command(&self) -> Vec<String> {
        self.privileged(&["v4l2loopback-ctl", "add", "-n", &self.config.card_label()])
    }

    /// Command that removes a loopback device during shutdown.
    pub fn remove_loopback_command(&self, device: &Path) -> Vec<String> {
        self.privileged(&["v4l2loopback-ctl", "delete", &device.to_string_lossy()])
    }

    /// ffmpeg command that turns the VM's VNC output into a V4L2 device.
    pub fn ffmpeg_command(&self, device: &Path) -> Vec<String> {
        vec![
            "ffmpeg".to_owned(),
            "-hide_banner".to_owned(),
            "-loglevel".to_owned(),
            "warning".to_owned(),
            "-f".to_owned(),
            "vnc".to_owned(),
            "-i".to_owned(),
            self.config.vnc_endpoint(),
            "-r".to_owned(),
            bounded_fps(self.config.fps).to_string(),
            "-pix_fmt".to_owned(),
            "yuyv422".to_owned(),
            "-f".to_owned(),
            "v4l2".to_owned(),
            device.to_string_lossy().into_owned(),
        ]
    }

    /// Load `v4l2loopback` if `lsmod` does not already report it.
    pub async fn ensure_module(&self) -> Result<(), ApiError> {
        if let Ok(output) = self.runner.run(&self.module_probe_command()).await {
            if output.success() && output.stdout.contains("v4l2loopback") {
                return Ok(());
            }
        }

        let output = self
            .runner
            .run(&self.ensure_module_command())
            .await
            .map_err(|error| module_error("run modprobe", error.to_string()))?;
        if output.success() {
            Ok(())
        } else {
            Err(module_error(
                "load the v4l2loopback module",
                command_failure(&output),
            ))
        }
    }

    /// Create the loopback device, falling back to `v4l2loopback-ctl add`.
    pub async fn create_loopback(&mut self, device_number: u16) -> Result<PathBuf, ApiError> {
        let requested = PathBuf::from(format!("/dev/video{device_number}"));

        match self
            .runner
            .run(&self.create_loopback_command(device_number))
            .await
        {
            Ok(output) if output.success() => {
                if self.await_device(&requested).await {
                    self.loopback_created = true;
                    return Ok(requested);
                }
                // modprobe can succeed without creating a node when the
                // module is already loaded. Fall back rather than returning a
                // path that does not exist.
                let primary = format!(
                    "modprobe succeeded but {} did not appear",
                    requested.display()
                );
                return self
                    .create_loopback_with_ctl(device_number, primary)
                    .await
                    .map(|path| {
                        self.loopback_created = true;
                        path
                    });
            }
            Ok(output) => {
                let primary = command_failure(&output);
                return self
                    .create_loopback_with_ctl(device_number, primary)
                    .await
                    .map(|path| {
                        self.loopback_created = true;
                        path
                    });
            }
            Err(error) => {
                return self
                    .create_loopback_with_ctl(device_number, format!("modprobe failed: {error}"))
                    .await
                    .map(|path| {
                        self.loopback_created = true;
                        path
                    });
            }
        }
    }

    /// Start ffmpeg for an already-created device.
    pub async fn start_ffmpeg(&mut self, device: &Path) -> Result<(), ApiError> {
        let argv = self.ffmpeg_command(device);
        match self.runner.spawn(&argv).await {
            Ok(child) => {
                self.child = Some(child);
                Ok(())
            }
            Err(error) => Err(ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!(
                    "Failed to start ffmpeg for virtual capture `{}`: {error}",
                    self.config.vm_name
                ),
            )
            .with_details(self.config.vnc_endpoint())
            .with_remediation(
                "Install ffmpeg with VNC input support, or let the controller stream VNC directly.",
            )
            .retryable()),
        }
    }

    /// Create the loopback device and start feeding VNC frames into it.
    ///
    /// Returns the device path. If ffmpeg cannot start, the loopback device is
    /// removed before the error is returned.
    pub async fn start(&mut self) -> Result<PathBuf, ApiError> {
        if let Some(path) = self.device_path.clone() {
            if self.child.is_some() {
                return Ok(path);
            }
        }

        self.validate_config()?;
        self.ensure_module().await?;

        let number = self
            .config
            .device_number
            .unwrap_or_else(|| next_device_number(&existing_video_numbers()));
        let path = self.create_loopback(number).await?;
        self.device_path = Some(path.clone());

        if let Err(error) = self.start_ffmpeg(&path).await {
            self.cleanup_loopback().await;
            self.device_path = None;
            return Err(error);
        }

        Ok(path)
    }

    /// Kill ffmpeg and remove the loopback device. Idempotent.
    pub async fn stop(&mut self) -> Result<(), ApiError> {
        let mut failure = None;

        if let Some(child) = self.child.take() {
            if let Err(error) = self.runner.kill(child).await {
                failure = Some(
                    ApiError::new(
                        codes::CAPTURE_UNAVAILABLE,
                        format!(
                            "Failed to stop ffmpeg for virtual capture `{}`: {error}",
                            self.config.vm_name
                        ),
                    )
                    .with_remediation("Kill the ffmpeg process manually, then retry stop.")
                    .retryable(),
                );
            }
        }

        self.cleanup_loopback().await;
        self.device_path = None;

        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn validate_config(&self) -> Result<(), ApiError> {
        if self.config.width == 0 || self.config.height == 0 {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                format!(
                    "Virtual capture for `{}` has empty geometry {}x{}.",
                    self.config.vm_name, self.config.width, self.config.height
                ),
            )
            .with_remediation("Configure a non-zero width and height."));
        }
        if self.config.vnc_host.is_empty() {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                "Virtual capture VNC host is empty.",
            )
            .with_remediation("Set vnc_host to the VM display's host (127.0.0.1 by default)."));
        }
        if self.config.vnc_port == 0 {
            return Err(
                ApiError::new(codes::INVALID_REQUEST, "Virtual capture VNC port is zero.")
                    .with_remediation("Set vnc_port to the VM's VNC port (5901 by default)."),
            );
        }
        Ok(())
    }

    async fn create_loopback_with_ctl(
        &mut self,
        device_number: u16,
        detail: String,
    ) -> Result<PathBuf, ApiError> {
        let output = self
            .runner
            .run(&self.create_loopback_ctl_command())
            .await
            .map_err(|error| loopback_error(&detail, error.to_string()))?;

        if !output.success() {
            return Err(loopback_error(&detail, command_failure(&output)));
        }

        // `v4l2loopback-ctl add` normally prints the device it created. If it
        // does not, keep the number requested for this capture rather than
        // allocating a second, different device.
        let reported = output.stdout.trim();
        let path = if reported.starts_with("/dev/") {
            PathBuf::from(reported)
        } else {
            PathBuf::from(format!("/dev/video{device_number}"))
        };

        if self.await_device(&path).await {
            Ok(path)
        } else {
            Err(loopback_error(
                &detail,
                format!("{} did not appear", path.display()),
            ))
        }
    }

    /// Wait briefly for udev/v4l2loopback to publish the device node.
    ///
    /// The bounds match the Python implementation's 20 x 100 ms wait, but with
    /// a shorter interval so a failed start remains responsive.
    async fn await_device(&self, path: &Path) -> bool {
        if (self.device_probe)(path) {
            return true;
        }
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if (self.device_probe)(path) {
                return true;
            }
        }
        false
    }

    async fn cleanup_loopback(&mut self) {
        if !self.loopback_created {
            return;
        }
        if let Some(device) = self.device_path.clone() {
            let _ = self
                .runner
                .run(&self.remove_loopback_command(&device))
                .await;
        }
        self.loopback_created = false;
    }

    fn privileged(&self, args: &[&str]) -> Vec<String> {
        self.privilege_prefix
            .iter()
            .cloned()
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect()
    }
}

/// Default sysfs root used to look up v4l2loopback card labels.
pub const VIRTUAL_CAPTURE_DEFAULT_SYS_ROOT: &str = "/sys/class/video4linux";

/// Default device root used to build `/dev/videoN` capability ids.
pub const VIRTUAL_CAPTURE_DEFAULT_DEV_ROOT: &str = "/dev";

/// Configuration for [`VirtualCaptureProvider`].
///
/// [`VirtualCaptureProviderConfig::capture`] holds the VNC endpoint, geometry,
/// frame rate, device number, and card-label prefix used by the existing
/// [`VirtualCapture`] lifecycle. `device_label` identifies the v4l2loopback
/// node during side-effect-free discovery. An empty `device_label` means
/// [`VirtualCaptureConfig::card_label`], which is derived from the VM name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VirtualCaptureProviderConfig {
    /// Lifecycle configuration for the loopback device and its ffmpeg feeder.
    pub capture: VirtualCaptureConfig,
    /// v4l2loopback card label to match in sysfs. Empty means the configured
    /// [`VirtualCaptureConfig::card_label`].
    pub device_label: String,
    /// sysfs root containing `video*/name` entries.
    pub sys_root: PathBuf,
    /// `/dev`-equivalent root used to form capability ids.
    pub dev_root: PathBuf,
}

impl Default for VirtualCaptureProviderConfig {
    fn default() -> Self {
        Self::new(VirtualCaptureConfig::default())
    }
}

impl VirtualCaptureProviderConfig {
    /// Configuration for one VM, using the host's sysfs and `/dev` roots.
    pub fn new(capture: VirtualCaptureConfig) -> Self {
        Self {
            capture,
            device_label: String::new(),
            sys_root: PathBuf::from(VIRTUAL_CAPTURE_DEFAULT_SYS_ROOT),
            dev_root: PathBuf::from(VIRTUAL_CAPTURE_DEFAULT_DEV_ROOT),
        }
    }

    /// Override the sysfs label used for discovery.
    pub fn with_device_label(mut self, label: impl Into<String>) -> Self {
        self.device_label = label.into();
        self
    }

    /// Override the device and sysfs roots (tests and unusual containers).
    pub fn with_roots(
        mut self,
        dev_root: impl Into<PathBuf>,
        sys_root: impl Into<PathBuf>,
    ) -> Self {
        self.dev_root = dev_root.into();
        self.sys_root = sys_root.into();
        self
    }

    /// Label used for discovery, falling back to the card label.
    pub fn label(&self) -> String {
        let label = self.device_label.trim();
        if label.is_empty() {
            self.capture.card_label()
        } else {
            label.to_owned()
        }
    }
}

/// Opens the underlying V4L2 device after [`VirtualCapture`] has created and
/// started it.
///
/// The provider calls this synchronously after the lifecycle reports success.
/// Tests inject a fake factory so opening a virtual source does not need a real
/// `/dev/videoN`.
pub trait VirtualSourceFactory: Send + Sync {
    /// Open the device at `device` as a capture source.
    fn open(&self, device: &Path) -> Result<Box<dyn CaptureSource>, ApiError>;
}

/// Production factory backed by the real V4L2 MMAP capture path.
#[derive(Clone, Copy, Debug, Default)]
pub struct V4l2SourceFactory;

impl VirtualSourceFactory for V4l2SourceFactory {
    fn open(&self, device: &Path) -> Result<Box<dyn CaptureSource>, ApiError> {
        let file_name = device
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                ApiError::new(
                    codes::INVALID_REQUEST,
                    format!(
                        "Virtual capture device path `{}` has no video node name.",
                        device.display()
                    ),
                )
            })?;
        let index = parse_video_index(file_name).ok_or_else(|| {
            ApiError::new(
                codes::INVALID_REQUEST,
                format!(
                    "Virtual capture device path `{}` is not a `/dev/videoN` node.",
                    device.display()
                ),
            )
            .with_details(device.display().to_string())
        })?;

        Ok(Box::new(V4l2Capture::open(V4l2Node {
            index,
            id: device.to_string_lossy().into_owned(),
            device_path: device.to_path_buf(),
            name: format!("V4L2 video{index}"),
            capture_like: true,
        })?))
    }
}

/// A capture provider that owns the v4l2loopback lifecycle for one VM display.
///
/// ## Discovery is sysfs-only
///
/// [`CaptureProvider::enumerate`] reads
/// `/sys/class/video4linux/video*/name` and matches the configured card label.
/// It never runs `lsmod`, `modprobe`, `v4l2loopback-ctl`, or `ffmpeg`; a
/// missing label is simply "no source". This mirrors the Python
/// `_find_v4l2loopback_device` lookup, including matching labels with spaces
/// normalised to underscores (and the reverse).
///
/// The capability is [`CaptureKind::V4l2`] because the concrete source is an
/// ordinary `/dev/videoN` device. [`crate::CaptureRegistry::with_virtual_capture`]
/// registers this provider ahead of a plain V4L2 provider so a labelled
/// loopback capability is opened through its lifecycle instead of as an
/// anonymous device.
///
/// ## Open starts the lifecycle
///
/// [`CaptureProvider::open`] starts the existing [`VirtualCapture`] lifecycle,
/// verifies the device index, and then asks the injected
/// [`VirtualSourceFactory`] for the source. If the factory fails, the lifecycle
/// is stopped before the error is returned, so no ffmpeg process or
/// v4l2loopback device is left behind.
pub struct VirtualCaptureProvider {
    config: VirtualCaptureProviderConfig,
    runner: Arc<dyn CommandRunner>,
    factory: Arc<dyn VirtualSourceFactory>,
    privilege_prefix: Vec<String>,
    device_probe: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
}

impl fmt::Debug for VirtualCaptureProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualCaptureProvider")
            .field("config", &self.config)
            .field("privilege_prefix", &self.privilege_prefix)
            .finish_non_exhaustive()
    }
}

impl Clone for VirtualCaptureProvider {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            runner: Arc::clone(&self.runner),
            factory: Arc::clone(&self.factory),
            privilege_prefix: self.privilege_prefix.clone(),
            device_probe: Arc::clone(&self.device_probe),
        }
    }
}

impl VirtualCaptureProvider {
    /// Create a provider using the production process runner, real V4L2
    /// factory, and `sudo` for module operations.
    pub fn new(config: VirtualCaptureProviderConfig) -> Self {
        Self::with_parts(
            config,
            Arc::new(ProcessCommandRunner::new()),
            Arc::new(|path: &Path| path.exists()),
        )
    }

    /// Create a provider with an injected process runner.
    ///
    /// As with [`VirtualCapture::with_runner`], an injected runner is assumed
    /// to model device creation, so the default probe trusts it. Call
    /// [`VirtualCaptureProvider::with_device_probe`] when a test needs a real
    /// filesystem check.
    pub fn with_runner(
        config: VirtualCaptureProviderConfig,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self::with_parts(config, runner, Arc::new(|_| true))
    }

    fn with_parts(
        config: VirtualCaptureProviderConfig,
        runner: Arc<dyn CommandRunner>,
        device_probe: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            config,
            runner,
            factory: Arc::new(V4l2SourceFactory),
            privilege_prefix: vec!["sudo".to_owned()],
            device_probe,
        }
    }

    /// Replace the factory that opens the underlying V4L2 device.
    pub fn with_factory(mut self, factory: Arc<dyn VirtualSourceFactory>) -> Self {
        self.factory = factory;
        self
    }

    /// Replace the privilege prefix (`sudo` by default).
    pub fn with_privilege_prefix<I, S>(mut self, prefix: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.privilege_prefix = prefix.into_iter().map(Into::into).collect();
        self
    }

    /// Run module operations without a privilege wrapper.
    pub fn without_privilege_prefix(self) -> Self {
        self.with_privilege_prefix(Vec::<String>::new())
    }

    /// Replace the check used to confirm that a loopback device exists.
    pub fn with_device_probe<F>(mut self, probe: F) -> Self
    where
        F: Fn(&Path) -> bool + Send + Sync + 'static,
    {
        self.device_probe = Arc::new(probe);
        self
    }

    /// Configuration this provider was built with.
    pub fn config(&self) -> &VirtualCaptureProviderConfig {
        &self.config
    }

    /// Label used for sysfs discovery.
    pub fn label(&self) -> String {
        self.config.label()
    }

    /// Find the labelled v4l2loopback device without starting anything.
    pub fn find_capability(&self) -> Result<Option<CaptureCapability>, ApiError> {
        let label = self.config.label();
        if label.trim().is_empty() {
            return Ok(None);
        }
        let variants = label_variants(&label);

        let entries = match fs::read_dir(&self.config.sys_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(sysfs_error(&self.config.sys_root, error)),
        };

        let mut matches: Vec<(u32, String, String)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| sysfs_error(&self.config.sys_root, error))?;
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            let Some(index) = parse_video_index(&file_name) else {
                continue;
            };

            let name = match fs::read_to_string(entry.path().join("name")) {
                Ok(name) => name.trim().to_owned(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(sysfs_error(&entry.path().join("name"), error)),
            };
            if name.is_empty() {
                continue;
            }
            if variants
                .iter()
                .any(|variant| name.contains(variant.as_str()))
            {
                matches.push((index, file_name.into_owned(), name));
            }
        }

        matches.sort_by_key(|(index, _, _)| *index);
        let Some((_, directory_name, name)) = matches.into_iter().next() else {
            return Ok(None);
        };

        let device_path = self.config.dev_root.join(directory_name);
        Ok(Some(CaptureCapability {
            id: device_path.to_string_lossy().into_owned(),
            name,
            kind: CaptureKind::V4l2,
            // sysfs does not expose geometry or formats; opening the device is
            // the only honest way to learn them.
            formats: Vec::new(),
            max_width: self.config.capture.width,
            max_height: self.config.capture.height,
            max_fps: bounded_fps(self.config.capture.fps),
            compressed: false,
            hdr: false,
            has_audio: false,
        }))
    }

    /// Alias for [`VirtualCaptureProvider::find_capability`].
    pub fn capability(&self) -> Result<Option<CaptureCapability>, ApiError> {
        self.find_capability()
    }

    fn lifecycle(&self, capture: VirtualCaptureConfig) -> VirtualCapture {
        let probe = Arc::clone(&self.device_probe);
        VirtualCapture::with_runner(capture, Arc::clone(&self.runner))
            .with_device_probe(move |path| (probe)(path))
            .with_privilege_prefix(self.privilege_prefix.clone())
    }
}

impl CaptureProvider for VirtualCaptureProvider {
    fn kind(&self) -> CaptureKind {
        // The underlying source really is a V4L2 device; only the lifecycle
        // that creates/feeds it is virtual.
        CaptureKind::V4l2
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(self.find_capability()?.into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::V4l2 {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                "The virtual capture provider cannot open a non-V4L2 capability.",
            ));
        }

        let Some(discovered) = self.find_capability()? else {
            return Err(not_found(
                capability,
                "no labelled v4l2loopback device is present",
            ));
        };
        if discovered.id != capability.id {
            return Err(not_found(
                capability,
                "the labelled v4l2loopback device has a different path",
            ));
        }

        let device_path = PathBuf::from(&capability.id);
        let index = device_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_video_index)
            .ok_or_else(|| {
                ApiError::new(
                    codes::INVALID_REQUEST,
                    format!(
                        "Virtual capture capability `{}` is not a `/dev/videoN` node.",
                        capability.id
                    ),
                )
                .with_details(capability.id.clone())
            })?;
        let number = u16::try_from(index).map_err(|_| {
            ApiError::new(
                codes::INVALID_REQUEST,
                format!("Virtual capture device number {index} is out of range."),
            )
            .with_details(capability.id.clone())
        })?;

        let mut capture_config = self.config.capture.clone();
        capture_config.device_number = Some(number);

        let lifecycle = self.lifecycle(capture_config);
        let (lifecycle, started, runtime) = start_lifecycle(lifecycle)?;

        // The lifecycle allocates a concrete `/dev/videoN`. Confirm it is the
        // node this capability described before opening it.
        let started_index = started
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(parse_video_index);
        if started_index != Some(index) {
            let _ = stop_lifecycle_blocking(lifecycle, runtime);
            return Err(ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!(
                    "Virtual capture started `{}` instead of the advertised `{}`.",
                    started.display(),
                    capability.id
                ),
            )
            .with_details(capability.id.clone())
            .with_remediation("Restart the VM profile and refresh capture discovery."));
        }

        match self.factory.open(&started) {
            Ok(inner) => Ok(Box::new(VirtualCaptureSource {
                inner,
                lifecycle: Some(lifecycle),
                runtime: Some(Arc::new(runtime)),
            })),
            Err(error) => {
                // Never leave a loopback device or ffmpeg behind when the
                // source cannot be opened.
                let _ = stop_lifecycle_blocking(lifecycle, runtime);
                Err(error)
            }
        }
    }
}

fn not_found(capability: &CaptureCapability, reason: &str) -> ApiError {
    ApiError::new(
        codes::NOT_FOUND,
        format!(
            "Virtual capture source `{}` is not available: {reason}.",
            capability.id
        ),
    )
    .with_details(capability.id.clone())
    .with_remediation("Refresh capture discovery after the VM display is running.")
}

fn label_variants(label: &str) -> Vec<String> {
    let mut variants = vec![label.to_owned()];
    let underscores = label.replace(' ', "_");
    if !variants.contains(&underscores) {
        variants.push(underscores);
    }
    let spaces = label.replace('_', " ");
    if !variants.contains(&spaces) {
        variants.push(spaces);
    }
    variants
}

fn sysfs_error(path: &Path, error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!(
            "Failed to inspect virtual capture metadata at {}: {error}",
            path.display()
        ),
    )
    .with_details(path.display().to_string())
    .with_remediation("Check permissions on /sys/class/video4linux and retry discovery.")
}

/// A [`CaptureSource`] that owns the v4l2loopback lifecycle feeding it.
///
/// Stopping the source stops the underlying capture, kills ffmpeg, and removes
/// the loopback device created by the provider.
pub struct VirtualCaptureSource {
    inner: Box<dyn CaptureSource>,
    lifecycle: Option<VirtualCapture>,
    runtime: Option<Arc<Runtime>>,
}

impl fmt::Debug for VirtualCaptureSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualCaptureSource")
            .field("id", &self.inner.id())
            .field("device_path", &self.device_path())
            .field("active", &self.active())
            .finish()
    }
}

impl VirtualCaptureSource {
    /// The `/dev/videoN` path backing this source, if the lifecycle still owns
    /// it.
    pub fn device_path(&self) -> Option<&Path> {
        self.lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.device_path())
    }

    /// True while the lifecycle still records a running ffmpeg child.
    pub fn active(&self) -> bool {
        self.lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.active())
    }
}

#[async_trait]
impl CaptureSource for VirtualCaptureSource {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn format(&self) -> FrameFormat {
        self.inner.format()
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        self.inner.next_frame().await
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let id = self.inner.id().to_owned();
        let inner_result = self.inner.stop().await;

        let lifecycle_result = match (self.lifecycle.take(), self.runtime.take()) {
            (Some(mut lifecycle), None) => lifecycle.stop().await.map_err(MediaError::from),
            (Some(mut lifecycle), Some(runtime)) => {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                std::thread::Builder::new()
                    .name("ozma-virtual-capture-stop".to_owned())
                    .spawn(move || {
                        let result = runtime.block_on(lifecycle.stop());
                        let _ = sender.send(result);
                    })
                    .map_err(|error| {
                        MediaError::capture(
                            &id,
                            format!("failed to start the lifecycle stop thread: {error}"),
                        )
                    })?;
                match receiver.await {
                    Ok(result) => result.map_err(MediaError::from),
                    Err(_) => Err(MediaError::capture(
                        &id,
                        "the virtual capture lifecycle stopped without reporting a result",
                    )),
                }
            }
            (None, _) => Ok(()),
        };

        // Cleanup failure is the more important signal: ffmpeg or the loopback
        // device may still be alive.
        match lifecycle_result {
            Ok(()) => inner_result,
            Err(error) => Err(error),
        }
    }
}

/// Drive `VirtualCapture::start` from the synchronous provider API.
///
/// The capture provider trait is synchronous but the lifecycle runs host
/// commands and waits for udev. A short-lived current-thread runtime on a
/// dedicated OS thread owns that work. The runtime is returned with the
/// lifecycle because ffmpeg's child handle stays on the runtime that spawned
/// it; stop drives the same runtime again.
fn start_lifecycle(
    mut lifecycle: VirtualCapture,
) -> Result<(VirtualCapture, PathBuf, Runtime), ApiError> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ozma-virtual-capture-start".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(ApiError::new(
                        codes::CAPTURE_UNAVAILABLE,
                        format!("Failed to create the virtual capture runtime: {error}"),
                    )));
                    return;
                }
            };
            match runtime.block_on(lifecycle.start()) {
                Ok(path) => {
                    let _ = sender.send(Ok((lifecycle, path, runtime)));
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                }
            }
        })
        .map_err(|error| {
            ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!("Failed to start the virtual capture worker thread: {error}"),
            )
        })?;

    receiver.recv().unwrap_or_else(|_| {
        Err(ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            "The virtual capture worker stopped before reporting a start result.",
        ))
    })
}

/// Stop a lifecycle from the synchronous `open` rollback path.
fn stop_lifecycle_blocking(
    mut lifecycle: VirtualCapture,
    runtime: Runtime,
) -> Result<(), ApiError> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("ozma-virtual-capture-rollback".to_owned())
        .spawn(move || {
            let result = runtime.block_on(lifecycle.stop());
            let _ = sender.send(result);
        })
        .map_err(|error| {
            ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!("Failed to start the virtual capture rollback thread: {error}"),
            )
        })?;

    receiver.recv().unwrap_or_else(|_| {
        Err(ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            "The virtual capture worker stopped before reporting a rollback result.",
        ))
    })
}

/// Clamp a configured frame rate to a sane ffmpeg output rate.
pub fn bounded_fps(fps: u32) -> u32 {
    fps.clamp(1, MAX_FPS)
}

/// Pick the first `/dev/videoN` number from 10 upward that is not in `existing`.
pub fn next_device_number(existing: &[u16]) -> u16 {
    let mut number = DEFAULT_DEVICE_NUMBER_START;
    while existing.contains(&number) {
        number = number.saturating_add(1);
    }
    number
}

/// Existing `/dev/videoN` numbers, or an empty list if `/dev` is unreadable.
pub fn existing_video_numbers() -> Vec<u16> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.strip_prefix("video")?.parse().ok()
        })
        .collect()
}

fn command_failure(output: &CommandOutput) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_owned();
    }
    match output.code {
        Some(code) => format!("exited with status {code}"),
        None => "terminated by a signal".to_owned(),
    }
}

fn module_error(action: &str, detail: impl Into<String>) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!("Failed to {action} for virtual capture: {}", detail.into()),
    )
    .with_remediation(
        "Install the v4l2loopback kernel module and allow the service to run modprobe, or configure privilege_prefix.",
    )
    .retryable()
}

fn loopback_error(primary: &str, fallback: impl Into<String>) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!(
            "Failed to create a v4l2loopback device (modprobe: {primary}; v4l2loopback-ctl: {}).",
            fallback.into()
        ),
    )
    .with_remediation(
        "Install v4l2loopback and v4l2loopback-ctl, or let the controller stream VNC directly.",
    )
    .retryable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct FakeRunner {
        runs: StdMutex<Vec<Vec<String>>>,
        spawns: StdMutex<Vec<Vec<String>>>,
        kills: StdMutex<Vec<ChildId>>,
        responses: StdMutex<VecDeque<std::io::Result<CommandOutput>>>,
        spawn_failure: StdMutex<Option<std::io::Error>>,
        next_child: AtomicU64,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self::default()
        }

        fn push_ok(&self, stdout: &str) {
            self.responses.lock().unwrap().push_back(Ok(CommandOutput {
                code: Some(0),
                stdout: stdout.to_owned(),
                stderr: String::new(),
            }));
        }

        fn push_fail(&self, stderr: &str) {
            self.responses.lock().unwrap().push_back(Ok(CommandOutput {
                code: Some(1),
                stdout: String::new(),
                stderr: stderr.to_owned(),
            }));
        }

        fn fail_spawn(&self, kind: std::io::ErrorKind, message: &str) {
            *self.spawn_failure.lock().unwrap() = Some(std::io::Error::new(kind, message));
        }

        fn run_commands(&self) -> Vec<Vec<String>> {
            self.runs.lock().unwrap().clone()
        }

        fn spawn_commands(&self) -> Vec<Vec<String>> {
            self.spawns.lock().unwrap().clone()
        }

        fn kill_count(&self) -> usize {
            self.kills.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
            self.runs.lock().unwrap().push(argv.to_vec());
            let scripted = self.responses.lock().unwrap().pop_front();
            scripted.unwrap_or_else(|| {
                Ok(CommandOutput {
                    code: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                })
            })
        }

        async fn spawn(&self, argv: &[String]) -> std::io::Result<ChildId> {
            self.spawns.lock().unwrap().push(argv.to_vec());
            if let Some(error) = self.spawn_failure.lock().unwrap().take() {
                return Err(error);
            }
            let id = self.next_child.fetch_add(1, Ordering::Relaxed) + 1;
            Ok(ChildId(id))
        }

        async fn kill(&self, child: ChildId) -> std::io::Result<()> {
            self.kills.lock().unwrap().push(child);
            Ok(())
        }
    }

    fn config() -> VirtualCaptureConfig {
        let mut config = VirtualCaptureConfig::new("vm1");
        config.device_number = Some(12);
        config
    }

    #[test]
    fn defaults_match_the_python_constructor() {
        let config = VirtualCaptureConfig::new("vm1");
        assert_eq!(config.vnc_host, "127.0.0.1");
        assert_eq!(config.vnc_port, 5901);
        assert_eq!(config.width, 1024);
        assert_eq!(config.height, 768);
        assert_eq!(config.fps, 20);
        assert_eq!(config.device_number, None);
        assert_eq!(config.card_label(), "Ozma Virtual vm1");
        assert_eq!(config.vnc_endpoint(), "127.0.0.1:5901");
    }

    #[test]
    fn argv_is_exact_without_a_privilege_prefix() {
        let capture = VirtualCapture::new(config()).without_privilege_prefix();
        let device = Path::new("/dev/video12");

        assert_eq!(capture.module_probe_command(), vec!["lsmod"]);
        assert_eq!(
            capture.ensure_module_command(),
            vec!["modprobe", "v4l2loopback"]
        );
        assert_eq!(
            capture.create_loopback_command(12),
            vec![
                "modprobe",
                "v4l2loopback",
                "video_nr=12",
                "card_label=Ozma Virtual vm1",
                "exclusive_caps=1",
            ]
        );
        assert_eq!(
            capture.create_loopback_ctl_command(),
            vec!["v4l2loopback-ctl", "add", "-n", "Ozma Virtual vm1"]
        );
        assert_eq!(
            capture.remove_loopback_command(device),
            vec!["v4l2loopback-ctl", "delete", "/dev/video12"]
        );
        assert_eq!(
            capture.ffmpeg_command(device),
            vec![
                "ffmpeg",
                "-hide_banner",
                "-loglevel",
                "warning",
                "-f",
                "vnc",
                "-i",
                "127.0.0.1:5901",
                "-r",
                "20",
                "-pix_fmt",
                "yuyv422",
                "-f",
                "v4l2",
                "/dev/video12",
            ]
        );
    }

    #[test]
    fn privilege_prefix_is_injectable_and_not_hard_coded() {
        let default = VirtualCapture::new(config());
        assert_eq!(
            default.ensure_module_command(),
            vec!["sudo", "modprobe", "v4l2loopback"]
        );
        assert!(
            !default
                .ffmpeg_command(Path::new("/dev/video12"))
                .contains(&"sudo".to_owned()),
            "ffmpeg must not be wrapped in the module privilege prefix"
        );

        let doas = VirtualCapture::new(config()).with_privilege_prefix(["doas"]);
        assert_eq!(
            doas.create_loopback_ctl_command(),
            vec!["doas", "v4l2loopback-ctl", "add", "-n", "Ozma Virtual vm1"]
        );
    }

    #[test]
    fn device_allocation_starts_at_ten_and_skips_existing() {
        assert_eq!(next_device_number(&[]), 10);
        assert_eq!(next_device_number(&[10, 11]), 12);
        assert_eq!(next_device_number(&[10, 12]), 11);
        assert_eq!(bounded_fps(0), 1);
        assert_eq!(bounded_fps(20), 20);
        assert_eq!(bounded_fps(10_000), MAX_FPS);
    }

    #[tokio::test]
    async fn start_and_stop_run_the_expected_lifecycle() {
        let runner = Arc::new(FakeRunner::new());
        runner.push_ok("v4l2loopback 12345 0");
        runner.push_ok("");

        let mut capture =
            VirtualCapture::with_runner(config(), runner.clone()).without_privilege_prefix();
        assert!(!capture.active());
        assert!(capture.device_path().is_none());

        let path = capture.start().await.unwrap();
        assert_eq!(path, PathBuf::from("/dev/video12"));
        assert_eq!(capture.device_path(), Some(Path::new("/dev/video12")));
        assert!(capture.active());

        let runs = runner.run_commands();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0], vec!["lsmod"]);
        assert_eq!(runs[1], capture.create_loopback_command(12));

        let spawns = runner.spawn_commands();
        assert_eq!(spawns.len(), 1);
        assert_eq!(spawns[0], capture.ffmpeg_command(&path));

        capture.stop().await.unwrap();
        assert!(!capture.active());
        assert!(capture.device_path().is_none());
        assert_eq!(runner.kill_count(), 1);
        assert_eq!(
            runner.run_commands().last().unwrap(),
            &capture.remove_loopback_command(&path)
        );

        // Idempotent: no second kill and no extra commands.
        let runs_before = runner.run_commands().len();
        capture.stop().await.unwrap();
        assert_eq!(runner.kill_count(), 1);
        assert_eq!(runner.run_commands().len(), runs_before);
    }

    #[tokio::test]
    async fn module_load_failure_is_typed_and_does_not_spawn() {
        let runner = Arc::new(FakeRunner::new());
        runner.push_ok(""); // lsmod does not list the module
        runner.push_fail("modprobe: FATAL: Module v4l2loopback not found");

        let mut capture =
            VirtualCapture::with_runner(config(), runner.clone()).without_privilege_prefix();
        let error = capture.start().await.unwrap_err();

        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("v4l2loopback"));
        assert!(error.remediation.is_some());
        assert!(!capture.active());
        assert!(runner.spawn_commands().is_empty());
    }

    #[tokio::test]
    async fn ctl_fallback_reports_the_device_it_created() {
        let runner = Arc::new(FakeRunner::new());
        runner.push_ok("v4l2loopback 12345 0");
        runner.push_fail("modprobe: FATAL: Module v4l2loopback is already loaded");
        runner.push_ok("/dev/video13\n");

        let mut capture =
            VirtualCapture::with_runner(config(), runner.clone()).without_privilege_prefix();
        let path = capture.start().await.unwrap();
        assert_eq!(path, PathBuf::from("/dev/video13"));
        assert_eq!(capture.device_path(), Some(Path::new("/dev/video13")));

        let runs = runner.run_commands();
        assert_eq!(runs[2], capture.create_loopback_ctl_command());
        assert_eq!(runner.spawn_commands().len(), 1);
    }

    #[tokio::test]
    async fn modprobe_without_a_device_falls_back_to_ctl() {
        let runner = Arc::new(FakeRunner::new());
        runner.push_ok("v4l2loopback 12345 0");
        runner.push_ok(""); // modprobe reports success but creates no node
        runner.push_ok("/dev/video13\n");

        let mut capture = VirtualCapture::with_runner(config(), runner.clone())
            .without_privilege_prefix()
            .with_device_probe(|path| path == Path::new("/dev/video13"));

        let path = capture.start().await.unwrap();
        assert_eq!(path, PathBuf::from("/dev/video13"));
        assert_eq!(capture.device_path(), Some(Path::new("/dev/video13")));

        let runs = runner.run_commands();
        assert_eq!(runs[1], capture.create_loopback_command(12));
        assert_eq!(runs[2], capture.create_loopback_ctl_command());
        assert_eq!(runner.spawn_commands().len(), 1);
    }

    #[tokio::test]
    async fn ffmpeg_start_failure_rolls_back_the_loopback() {
        let runner = Arc::new(FakeRunner::new());
        runner.push_ok("v4l2loopback 12345 0");
        runner.push_ok("");
        runner.fail_spawn(std::io::ErrorKind::NotFound, "No such file or directory");

        let mut capture =
            VirtualCapture::with_runner(config(), runner.clone()).without_privilege_prefix();
        let error = capture.start().await.unwrap_err();

        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("ffmpeg"));
        assert!(error.remediation.as_deref().unwrap().contains("VNC"));
        assert!(!capture.active());
        assert!(capture.device_path().is_none());

        let runs = runner.run_commands();
        assert_eq!(
            runs.last().unwrap(),
            &vec![
                "v4l2loopback-ctl".to_owned(),
                "delete".to_owned(),
                "/dev/video12".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn stop_before_start_is_ok() {
        let runner = Arc::new(FakeRunner::new());
        let mut capture = VirtualCapture::with_runner(config(), runner);
        capture.stop().await.unwrap();
        capture.stop().await.unwrap();
        assert!(!capture.active());
    }

    #[tokio::test]
    async fn stop_reports_kill_failure_but_still_cleans_up() {
        struct FailingKill(FakeRunner);

        #[async_trait]
        impl CommandRunner for FailingKill {
            async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
                self.0.run(argv).await
            }

            async fn spawn(&self, argv: &[String]) -> std::io::Result<ChildId> {
                self.0.spawn(argv).await
            }

            async fn kill(&self, _child: ChildId) -> std::io::Result<()> {
                Err(std::io::Error::other("kill denied"))
            }
        }

        let runner = Arc::new(FailingKill(FakeRunner::new()));
        runner.0.push_ok("v4l2loopback 12345 0");
        runner.0.push_ok("");

        let mut capture =
            VirtualCapture::with_runner(config(), runner.clone()).without_privilege_prefix();
        capture.start().await.unwrap();

        let error = capture.stop().await.unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("kill denied"));
        assert!(!capture.active());
        assert!(capture.device_path().is_none());
        assert_eq!(
            runner.0.run_commands().last().unwrap()[0],
            "v4l2loopback-ctl"
        );
    }

    fn write_sysfs_label(sys_root: &Path, index: u32, name: &str) {
        let directory = sys_root.join(format!("video{index}"));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("name"), name).unwrap();
    }

    fn provider_config(sys_root: &Path) -> VirtualCaptureProviderConfig {
        VirtualCaptureProviderConfig::new(config()).with_roots(PathBuf::from("/dev"), sys_root)
    }

    #[derive(Default)]
    struct FakeFactory {
        fail: bool,
        calls: StdMutex<Vec<PathBuf>>,
    }

    impl VirtualSourceFactory for FakeFactory {
        fn open(&self, device: &Path) -> Result<Box<dyn CaptureSource>, ApiError> {
            self.calls.lock().unwrap().push(device.to_path_buf());
            if self.fail {
                return Err(ApiError::new(
                    codes::CAPTURE_UNAVAILABLE,
                    "injected factory failure",
                ));
            }
            let provider = crate::fake::FakeCaptureProvider::new();
            let capability = provider.enumerate().unwrap().remove(0);
            provider.open(&capability)
        }
    }

    #[test]
    fn provider_discovers_a_matching_label_without_starting_a_process() {
        let directory = tempfile::tempdir().unwrap();
        let sys_root = directory.path().join("sys");
        write_sysfs_label(&sys_root, 12, "Ozma Virtual vm1\n");
        write_sysfs_label(&sys_root, 13, "USB Camera\n");

        let runner = Arc::new(FakeRunner::new());
        let provider =
            VirtualCaptureProvider::with_runner(provider_config(&sys_root), runner.clone())
                .without_privilege_prefix();

        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        let capability = &capabilities[0];
        assert_eq!(capability.id, "/dev/video12");
        assert_eq!(capability.name, "Ozma Virtual vm1");
        assert_eq!(capability.kind, CaptureKind::V4l2);
        assert_eq!((capability.max_width, capability.max_height), (1024, 768));
        assert_eq!(capability.max_fps, 20);
        assert!(runner.run_commands().is_empty());
        assert!(runner.spawn_commands().is_empty());

        // v4l2loopback may normalise spaces to underscores.
        let underscored_root = directory.path().join("sys-underscored");
        write_sysfs_label(&underscored_root, 14, "Ozma_Virtual_vm1\n");
        let underscored = VirtualCaptureProvider::with_runner(
            provider_config(&underscored_root),
            Arc::new(FakeRunner::new()),
        );
        assert_eq!(underscored.enumerate().unwrap().len(), 1);

        // Missing sysfs is "no source", not an error.
        let missing = VirtualCaptureProvider::with_runner(
            provider_config(&directory.path().join("absent")),
            Arc::new(FakeRunner::new()),
        );
        assert!(missing.enumerate().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_open_starts_the_lifecycle_and_returns_the_factory_source() {
        let directory = tempfile::tempdir().unwrap();
        let sys_root = directory.path().join("sys");
        write_sysfs_label(&sys_root, 12, "Ozma Virtual vm1\n");

        let runner = Arc::new(FakeRunner::new());
        runner.push_ok("v4l2loopback 12345 0");
        let factory = Arc::new(FakeFactory::default());
        let provider =
            VirtualCaptureProvider::with_runner(provider_config(&sys_root), runner.clone())
                .with_factory(factory.clone())
                .without_privilege_prefix();

        let capability = provider.enumerate().unwrap().remove(0);
        let mut source = provider.open(&capability).unwrap();
        assert_eq!(source.id(), "fake0");
        assert_eq!(
            factory.calls.lock().unwrap().as_slice(),
            [PathBuf::from("/dev/video12")]
        );

        let runs = runner.run_commands();
        assert_eq!(runs[0], vec!["lsmod"]);
        assert_eq!(
            runs[1],
            vec![
                "modprobe",
                "v4l2loopback",
                "video_nr=12",
                "card_label=Ozma Virtual vm1",
                "exclusive_caps=1",
            ]
        );
        let spawns = runner.spawn_commands();
        assert_eq!(spawns.len(), 1);
        assert_eq!(
            spawns[0],
            VirtualCapture::new(config()).ffmpeg_command(Path::new("/dev/video12"))
        );

        source.stop().await.unwrap();
        assert_eq!(runner.kill_count(), 1);
        assert_eq!(
            runner.run_commands().last().unwrap(),
            &vec![
                "v4l2loopback-ctl".to_owned(),
                "delete".to_owned(),
                "/dev/video12".to_owned(),
            ]
        );
    }

    #[test]
    fn provider_open_rolls_back_the_lifecycle_when_the_factory_fails() {
        let directory = tempfile::tempdir().unwrap();
        let sys_root = directory.path().join("sys");
        write_sysfs_label(&sys_root, 12, "Ozma Virtual vm1\n");

        let runner = Arc::new(FakeRunner::new());
        runner.push_ok("v4l2loopback 12345 0");
        let provider =
            VirtualCaptureProvider::with_runner(provider_config(&sys_root), runner.clone())
                .with_factory(Arc::new(FakeFactory {
                    fail: true,
                    calls: StdMutex::new(Vec::new()),
                }))
                .without_privilege_prefix();

        let capability = provider.enumerate().unwrap().remove(0);
        let error = provider
            .open(&capability)
            .err()
            .expect("factory failure must fail open");
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("injected factory failure"));

        assert_eq!(runner.kill_count(), 1);
        assert_eq!(
            runner.run_commands().last().unwrap(),
            &vec![
                "v4l2loopback-ctl".to_owned(),
                "delete".to_owned(),
                "/dev/video12".to_owned(),
            ]
        );
    }

    #[test]
    fn provider_open_rejects_a_mismatched_kind_or_id() {
        let directory = tempfile::tempdir().unwrap();
        let sys_root = directory.path().join("sys");
        write_sysfs_label(&sys_root, 12, "Ozma Virtual vm1\n");
        let provider = VirtualCaptureProvider::with_runner(
            provider_config(&sys_root),
            Arc::new(FakeRunner::new()),
        );
        let capability = provider.enumerate().unwrap().remove(0);

        let mut wrong_kind = capability.clone();
        wrong_kind.kind = CaptureKind::Fake;
        assert_eq!(
            provider
                .open(&wrong_kind)
                .err()
                .expect("wrong kind must fail")
                .code,
            codes::NOT_FOUND
        );

        let mut wrong_id = capability;
        wrong_id.id = "/dev/video99".to_owned();
        assert_eq!(
            provider
                .open(&wrong_id)
                .err()
                .expect("wrong id must fail")
                .code,
            codes::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn provider_source_stop_propagates_a_lifecycle_kill_failure() {
        struct FailingKill(FakeRunner);

        #[async_trait]
        impl CommandRunner for FailingKill {
            async fn run(&self, argv: &[String]) -> std::io::Result<CommandOutput> {
                self.0.run(argv).await
            }

            async fn spawn(&self, argv: &[String]) -> std::io::Result<ChildId> {
                self.0.spawn(argv).await
            }

            async fn kill(&self, _child: ChildId) -> std::io::Result<()> {
                Err(std::io::Error::other("kill denied"))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let sys_root = directory.path().join("sys");
        write_sysfs_label(&sys_root, 12, "Ozma Virtual vm1\n");

        let runner = Arc::new(FailingKill(FakeRunner::new()));
        runner.0.push_ok("v4l2loopback 12345 0");
        let provider =
            VirtualCaptureProvider::with_runner(provider_config(&sys_root), runner.clone())
                .with_factory(Arc::new(FakeFactory::default()))
                .without_privilege_prefix();

        let capability = provider.enumerate().unwrap().remove(0);
        let mut source = provider.open(&capability).unwrap();
        let error = source.stop().await.unwrap_err();
        let api = error.to_api_error();
        assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
        assert!(api.message.contains("kill denied"));
        assert_eq!(
            runner.0.run_commands().last().unwrap()[0],
            "v4l2loopback-ctl"
        );
    }

    #[tokio::test]
    #[ignore = "requires v4l2loopback, ffmpeg, and an existing labelled device"]
    async fn real_v4l2loopback_provider_opens_an_existing_device() {
        let provider = VirtualCaptureProvider::new(VirtualCaptureProviderConfig::new(
            VirtualCaptureConfig::new("ozma-provider-test"),
        ));
        let capabilities = provider.enumerate().unwrap();
        let Some(capability) = capabilities.into_iter().next() else {
            return;
        };

        let mut source = provider.open(&capability).unwrap();
        let _ = source.next_frame().await;
        source.stop().await.unwrap();
    }
}
