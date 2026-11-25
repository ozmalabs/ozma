// SPDX-License-Identifier: AGPL-3.0-only
//! Capture providers and the registry that enumerates them.
//!
//! Enumeration never opens a device. That matters for two reasons: it lets a
//! node advertise capabilities while idle, and it means a broken source cannot
//! take down discovery for every other source.

use std::sync::Arc;

use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};

use ozma_media::CaptureSource;

/// Advance a monotonically increasing frame sequence number.
pub fn next_sequence(previous: Option<u64>) -> u64 {
    previous.map_or(0, |sequence| sequence.saturating_add(1))
}

/// Keep a PTS sequence strictly increasing.
///
/// Drivers occasionally stamp two buffers with the same time or their clock
/// restarts. PTS is part of the capture contract, so nudge a non-increasing
/// candidate forward by one microsecond instead of passing it through.
pub fn monotonic_pts(candidate: u64, previous: Option<u64>) -> u64 {
    match previous {
        Some(previous) => candidate.max(previous.saturating_add(1)),
        None => candidate,
    }
}

/// A backend that can enumerate and open capture sources.
///
/// Implementations are synchronous because discovery is filesystem/kernel
/// metadata work, not media streaming. Opening a device returns an async
/// [`CaptureSource`].
pub trait CaptureProvider: Send + Sync {
    /// Capture kind this provider owns.
    fn kind(&self) -> CaptureKind;

    /// Enumerate sources without opening them.
    ///
    /// A provider should return an empty list when the platform simply has no
    /// devices of this kind, and an error when discovery itself failed.
    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError>;

    /// Open a previously enumerated source.
    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError>;
}

/// Result of asking one provider to enumerate.
///
/// A failing provider is reported here rather than aborting the whole scan, so
/// one missing kernel module does not hide every other source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureScan {
    /// Provider kind that produced this result.
    pub kind: CaptureKind,
    /// Sources discovered by the provider.
    pub capabilities: Vec<CaptureCapability>,
    /// Discovery failure, if any.
    pub error: Option<ApiError>,
}

impl CaptureScan {
    /// True when discovery completed without an error.
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// A registry of capture providers.
///
/// Providers are held behind `Arc` so a caller can clone the registry into a
/// capability-reporting task without ambient state.
#[derive(Default, Clone)]
pub struct CaptureRegistry {
    providers: Vec<Arc<dyn CaptureProvider>>,
}

impl std::fmt::Debug for CaptureRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureRegistry")
            .field("providers", &self.providers.len())
            .finish()
    }
}

impl CaptureRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry with the host's native discovery provider (V4L2 on Linux,
    /// desktop GDI capture on Windows, empty otherwise).
    pub fn system() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::new().with(crate::v4l2::V4l2Provider::new())
        }
        #[cfg(target_os = "windows")]
        {
            Self::new().with(crate::windows_display::WindowsDisplayProvider::new())
        }
        #[cfg(target_os = "macos")]
        {
            Self::new().with(crate::macos_display::MacOsDisplayProvider::new())
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            Self::new()
        }
    }

    /// A registry with the host's V4L2 provider and, on Linux, the PipeWire
    /// video provider.
    ///
    /// V4L2 stays registered: HDMI grabbers, MJPEG devices, and codec
    /// pass-through still belong to the kernel path, while PipeWire covers
    /// cameras and portal/screen-cast nodes.  On other platforms this is the
    /// same as [`CaptureRegistry::system`].
    pub fn system_with_pipewire() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self::new()
                .with(crate::v4l2::V4l2Provider::new())
                .with(crate::pipewire::PipeWireCaptureProvider::new())
        }

        #[cfg(not(target_os = "linux"))]
        {
            Self::system()
        }
    }

    /// Add a provider. Providers are consulted in registration order.
    pub fn register<P>(&mut self, provider: P)
    where
        P: CaptureProvider + 'static,
    {
        self.providers.push(Arc::new(provider));
    }

    /// Builder form of [`CaptureRegistry::register`].
    pub fn with<P>(mut self, provider: P) -> Self
    where
        P: CaptureProvider + 'static,
    {
        self.register(provider);
        self
    }

    /// Add a QMP `screendump` display provider.
    ///
    /// This is deliberately not part of [`CaptureRegistry::system`] or
    /// [`CaptureRegistry::system_with_pipewire`]: a hardware node has no QMP
    /// socket, and a VM node's socket is profile configuration. Call this from
    /// the VM profile (or a test) with the socket path it owns.
    pub fn with_qmp_display(mut self, config: crate::qmp_display::QmpDisplayConfig) -> Self {
        self.register(crate::qmp_display::QmpDisplayProvider::new(config));
        self
    }

    /// Add a QMP `screendump` provider that shares the node's control client.
    ///
    /// QEMU accepts one client per `-qmp` chardev. When the node owns a single
    /// [`ozma_qmp::SharedQmpClient`] for power, capture, snapshots, and input,
    /// register the display provider with this method so HLS and snapshot
    /// sources never open a second control connection.
    pub fn with_qmp_display_shared(
        mut self,
        config: crate::qmp_display::QmpDisplayConfig,
        shared: ozma_qmp::SharedQmpClient,
    ) -> Self {
        self.register(crate::qmp_display::QmpDisplayProvider::with_shared_client(
            config, shared,
        ));
        self
    }

    /// Add an ffmpeg-backed VNC display provider.
    ///
    /// This is opt-in for the same reason as [`CaptureRegistry::with_qmp_display`]:
    /// a hardware/desktop node has no QEMU VNC endpoint. Enumeration only
    /// validates the configuration; it does not connect or spawn ffmpeg.
    pub fn with_vnc_display(mut self, config: crate::vnc_display::VncDisplayConfig) -> Self {
        self.register(crate::vnc_display::VncDisplayProvider::new(config));
        self
    }

    /// Add a QEMU D-Bus display provider.
    ///
    /// Like [`CaptureRegistry::with_qmp_display`], this is opt-in: the QMP
    /// socket and `-display dbus,p2p=yes` belong to VM profile configuration,
    /// not a hardware node. Enumeration only checks the socket path.
    #[cfg(target_os = "linux")]
    pub fn with_dbus_display(mut self, config: crate::dbus_display::DBusDisplayConfig) -> Self {
        self.register(crate::dbus_display::DBusDisplayProvider::new(config));
        self
    }

    /// Add a QEMU D-Bus display provider that shares the node's control client.
    ///
    /// QEMU accepts one client per `-qmp` chardev. When the node owns a single
    /// [`ozma_qmp::SharedQmpClient`], register the D-Bus provider with this
    /// method so its `getfd`/`add_client` handoff never opens a second control
    /// connection.
    #[cfg(target_os = "linux")]
    pub fn with_dbus_display_shared(
        mut self,
        config: crate::dbus_display::DBusDisplayConfig,
        shared: ozma_qmp::SharedQmpClient,
    ) -> Self {
        self.register(crate::dbus_display::DBusDisplayProvider::with_shared_client(config, shared));
        self
    }

    /// Add a Looking Glass KVMFR provider.
    ///
    /// Like the QMP provider, this is opt-in: a hardware node has no IVSHMEM
    /// region. Discovery still only reads the shared-memory header.
    #[cfg(target_os = "linux")]
    pub fn with_looking_glass(
        mut self,
        config: crate::lookingglass::LookingGlassProviderConfig,
    ) -> Self {
        self.register(crate::lookingglass::LookingGlassProvider::new(config));
        self
    }

    /// Add a v4l2loopback virtual-capture provider.
    ///
    /// The capability is [`CaptureKind::V4l2`], but opening it has to run the
    /// loopback/ffmpeg lifecycle rather than opening an anonymous device. The
    /// provider is therefore inserted ahead of any existing V4L2 provider, so
    /// its labelled capability is offered to the virtual provider first; a
    /// capability it does not own returns `not_found` and discovery falls
    /// through to the plain V4L2 provider.
    ///
    /// This is deliberately not part of [`CaptureRegistry::system`] or
    /// [`CaptureRegistry::system_with_pipewire`].
    #[cfg(target_os = "linux")]
    pub fn with_virtual_capture(
        mut self,
        config: crate::r#virtual::VirtualCaptureProviderConfig,
    ) -> Self {
        self.providers.insert(
            0,
            Arc::new(crate::r#virtual::VirtualCaptureProvider::new(config)),
        );
        self
    }

    /// Number of registered providers.
    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    /// Ask every provider for its sources, preserving per-provider failures.
    ///
    /// This is the diagnostics-friendly view. Use [`CaptureRegistry::enumerate`]
    /// when a failure should stop the caller.
    pub fn scan(&self) -> Vec<CaptureScan> {
        self.providers
            .iter()
            .map(|provider| match provider.enumerate() {
                Ok(capabilities) => CaptureScan {
                    kind: provider.kind(),
                    capabilities,
                    error: None,
                },
                Err(error) => CaptureScan {
                    kind: provider.kind(),
                    capabilities: Vec::new(),
                    error: Some(error),
                },
            })
            .collect()
    }

    /// Enumerate every source, failing on the first provider error.
    pub fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        let mut all = Vec::new();
        for scan in self.scan() {
            if let Some(error) = scan.error {
                return Err(error);
            }
            all.extend(scan.capabilities);
        }
        Ok(all)
    }

    /// Open an enumerated source.
    ///
    /// The provider with the matching kind is asked first; a provider that
    /// reports `not_found` does not mask a genuine backend failure from another
    /// provider.
    pub fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        for provider in &self.providers {
            if provider.kind() != capability.kind {
                continue;
            }
            match provider.open(capability) {
                Ok(source) => return Ok(source),
                Err(error) if error.code == codes::NOT_FOUND => continue,
                Err(error) => return Err(error),
            }
        }
        Err(ApiError::new(
            codes::NOT_FOUND,
            format!("No capture provider owns source `{}`.", capability.id),
        )
        .with_details(capability.id.clone())
        .with_remediation("Refresh capture discovery and retry with a current source id."))
    }

    /// Enumerate and then open the source with `id`.
    pub fn open_id(&self, id: &str) -> Result<Box<dyn CaptureSource>, ApiError> {
        let capability = self
            .enumerate()?
            .into_iter()
            .find(|capability| capability.id == id)
            .ok_or_else(|| {
                ApiError::new(
                    codes::NOT_FOUND,
                    format!("Capture source `{id}` was not found."),
                )
                .with_details(id.to_owned())
                .with_remediation("Check the device is plugged in and visible to the OS.")
            })?;
        self.open(&capability)
    }
}
