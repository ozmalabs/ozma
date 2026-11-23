// SPDX-License-Identifier: AGPL-3.0-only
//! Capture providers and the registry that enumerates them.
//!
//! Enumeration never opens a device. That matters for two reasons: it lets a
//! node advertise capabilities while idle, and it means a broken source cannot
//! take down discovery for every other source.

use std::sync::Arc;

use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};

use ozma_media::CaptureSource;

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

    /// A registry with the host's V4L2 discovery provider.
    pub fn system() -> Self {
        Self::new().with(crate::v4l2::V4l2Provider::new())
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
