// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! One-shot display snapshot support for the controller compatibility surface.
//!
//! The Python controller fetches `GET /display/snapshot` and expects a raw
//! `image/jpeg` body (`controller/api.py`, `controller/stream.py`, and
//! `controller/terminal_bridge.py`). This module opens the node's configured
//! capture source once, reads exactly one [`Frame`], and encodes it with
//! [`ozma_encode::encode_jpeg_frame`].
//!
//! Capture is deliberately one-shot by default: opening a QMP/D-Bus/VNC source
//! for every request is slower than a framebuffer tap, but it avoids inventing a
//! shared frame protocol and never claims a stream that the node is not
//! producing. The bounded per-console broker
//! ([`crate::display_broker::ConsoleBroker`]) opts into
//! [`SnapshotControl::with_source_cache`] for non-active consoles, so repeated
//! `?console=` previews reuse one provider connection while the default active
//! route stays one-shot. Enumeration and frame production are bounded by a
//! timeout supplied by the caller; a missing source or a busy device becomes a
//! typed error that the HTTP layer maps to a 503 with remediation.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::http::StatusCode;
use ozma_capture::CaptureRegistry;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, CaptureCapability};
use serde::Serialize;

/// Default bound for opening a source, reading one frame, and encoding it.
pub const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
/// Default JPEG quality for controller previews.
pub const DEFAULT_JPEG_QUALITY: u8 = 85;

/// Facts returned by `GET /display/info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DisplayInfo {
    /// The info document itself is available even when no source is configured.
    pub ok: bool,
    /// Capture source id, when one is configured/enumerated.
    pub source_id: Option<String>,
    /// Human-readable source name.
    pub source_name: Option<String>,
    /// Last captured width. `0` until a real frame has been observed; the
    /// advertised capability limit is exposed separately in
    /// `capability_max_width`.
    pub width: u32,
    /// Last captured height. `0` until a real frame has been observed; the
    /// advertised capability limit is exposed separately in
    /// `capability_max_height`.
    pub height: u32,
    /// Last captured pixel format, or the first advertised format.
    pub pixel_format: Option<String>,
    /// Widest frame the selected source advertises, when known. This is a
    /// capability, never the current frame size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_max_width: Option<u32>,
    /// Tallest frame the selected source advertises, when known. This is a
    /// capability, never the current frame size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_max_height: Option<u32>,
    /// Whether a source exists that the snapshot route can try to open.
    pub snapshot_available: bool,
    /// Always `jpeg`; the controller expects a JPEG body.
    pub snapshot_encoding: &'static str,
    /// Where `width`/`height` came from: `last-frame`, `display-output`, or
    /// `unknown`/`none` when no frame has been observed.
    pub dimensions_source: &'static str,
    /// Most recent snapshot failure, kept honest instead of implying success.
    pub last_error: Option<String>,
    /// Console index this document describes, when the node advertises VM
    /// consoles. `None` keeps the host-output/backward-compatible shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub console: Option<u32>,
    /// True when this console is the active HLS/WebRTC capture console.
    #[serde(default)]
    pub active: bool,
}

/// A successfully encoded snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFrame {
    /// Raw JPEG bytes (`FF D8 ...`).
    pub jpeg: Vec<u8>,
    /// Source that produced the frame.
    pub source_id: String,
    /// Source display name.
    pub source_name: String,
    /// Geometry/layout of the captured frame.
    pub format: FrameFormat,
}

/// Errors from the one-shot snapshot path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    /// No [`SnapshotControl`] was attached to the compatibility state.
    #[error("display snapshot support is not configured")]
    NotConfigured,
    /// No capture source is configured or discoverable.
    #[error("no capture source is available for display snapshots")]
    NoSource {
        /// Explicit source id that was requested, when any.
        requested: Option<String>,
    },
    /// A provider could not enumerate or open a source.
    #[error("opening a capture source failed: {0}")]
    Open(ApiError),
    /// The source was opened but produced no frame.
    #[error("capture source `{source_id}` failed: {error}")]
    Capture {
        /// Source id.
        source_id: String,
        /// Underlying media failure.
        error: MediaError,
    },
    /// Source opening or frame production exceeded the configured bound.
    #[error("capture source `{source_id}` did not produce a frame within {timeout:?}")]
    Timeout {
        /// Source id, or an empty string when enumeration timed out.
        source_id: String,
        /// Bound that elapsed.
        timeout: Duration,
    },
    /// A frame was captured but could not be encoded.
    #[error("encoding a display snapshot failed: {0}")]
    Encode(ozma_encode::JpegError),
    /// JPEG encoding exceeded the configured bound.
    #[error("display snapshot encoding did not finish within {timeout:?}")]
    EncodeTimeout {
        /// Bound that elapsed.
        timeout: Duration,
    },
    /// The blocking capture task failed to join.
    #[error("display snapshot task failed: {0}")]
    Task(String),
    /// A console query named a console that the node does not advertise.
    #[error("console {requested:?} is not among the advertised display outputs")]
    UnknownConsole {
        /// Raw query value, when one was supplied.
        requested: Option<String>,
        /// Advertised console indexes.
        available: Vec<u32>,
    },
    /// The configured provider exposes one default head and cannot capture
    /// another console independently.
    #[error(
        "the {provider} capture provider exposes one default console and cannot capture \
         Console_{requested} independently"
    )]
    SingleHead {
        /// Provider name (`qmp`, `vnc`, `looking_glass`, ...).
        provider: String,
        /// Console the provider actually captures.
        active: u32,
        /// Console that was requested.
        requested: u32,
    },
    /// A console query was supplied but no per-console broker/selector exists.
    #[error("multi-console snapshots are not configured on this node")]
    ConsoleUnsupported,
}

impl SnapshotError {
    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::EncodeTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::Task(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::UnknownConsole { .. } => StatusCode::NOT_FOUND,
            Self::SingleHead { .. } => StatusCode::CONFLICT,
            Self::ConsoleUnsupported => StatusCode::NOT_IMPLEMENTED,
            Self::NotConfigured
            | Self::NoSource { .. }
            | Self::Open(_)
            | Self::Capture { .. }
            | Self::Encode(_) => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// Typed error body with a stable code, message, and actionable remediation.
    pub fn api_error(&self) -> ApiError {
        match self {
            Self::NotConfigured => ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                "Display snapshots are not configured on this node.",
            )
            .with_remediation(
                "Start the node with a video capture source (for example --qmp-socket, \
                 --vnc-host/--vnc-port, or a V4L2/PipeWire device).",
            ),
            Self::NoSource { requested } => {
                let message = match requested {
                    Some(id) => format!("Capture source `{id}` is not available."),
                    None => "No capture source is available for display snapshots.".to_owned(),
                };
                ApiError::new(codes::CAPTURE_UNAVAILABLE, message)
                    .with_details(requested.clone().unwrap_or_default())
                    .with_remediation(
                        "Check GET /display/info and the node's capture flags; a VM node needs \
                         --qmp-socket, --vnc-host/--vnc-port, or D-Bus display flags.",
                    )
            }
            Self::Open(error) => {
                let remediation = error.remediation.clone().unwrap_or_else(|| {
                    "Check that the capture device or QMP/VNC/D-Bus socket is present and not \
                     already held open by another process."
                        .to_owned()
                });
                ApiError::new(
                    if error.code == codes::NOT_FOUND {
                        codes::CAPTURE_UNAVAILABLE.to_owned()
                    } else {
                        error.code.clone()
                    },
                    error.message.clone(),
                )
                .with_details(error.details.clone())
                .with_remediation(remediation)
                .retryable()
            }
            Self::Capture { source_id, error } => {
                let mut api = error.to_api_error();
                api.details = serde_json::Value::String(source_id.clone());
                if api.remediation.is_none() {
                    api.remediation = Some(
                        "The capture backend is present but produced no frame. Check the QMP/VNC/\
                         D-Bus socket, the device permissions, and whether the display is active."
                            .to_owned(),
                    );
                }
                api
            }
            Self::Timeout { source_id, timeout } => ApiError::new(
                "display.snapshot_timeout",
                if source_id.is_empty() {
                    format!("Capture discovery did not finish within {timeout:?}.")
                } else {
                    format!("Capture source `{source_id}` did not produce a frame within {timeout:?}.")
                },
            )
            .with_remediation(
                "Check that the QMP/VNC/D-Bus socket or capture device is responsive and not \
                 busy, then retry; increase the snapshot timeout only if the backend is known slow.",
            )
            .retryable(),
            Self::Encode(ozma_encode::JpegError::UnsupportedFormat(format)) => ApiError::new(
                "display.unsupported_format",
                format!(
                    "The capture source produces `{format}` frames, which cannot be encoded as JPEG."
                ),
            )
            .with_details(format.as_str())
            .with_remediation(
                "Use a source that emits rgb24, bgra, mjpeg, yuyv, or nv12 (QMP, D-Bus, VNC, \
                 Looking Glass, or a compatible V4L2 device).",
            ),
            Self::Encode(error) => ApiError::new("display.encode_failed", error.to_string())
                .with_remediation("Retry the snapshot; check the node log for the encoder error."),
            Self::EncodeTimeout { timeout } => ApiError::new(
                "display.encode_timeout",
                format!("JPEG encoding did not finish within {timeout:?}."),
            )
            .with_remediation(
                "Retry the snapshot; a frame larger than the configured capture size or a \
                 heavily loaded host can exceed the encode bound.",
            )
            .retryable(),
            Self::Task(error) => ApiError::new(codes::INTERNAL, error.clone())
                .with_remediation("Retry the snapshot; check the node log for the task failure."),
            Self::UnknownConsole {
                requested,
                available,
            } => ApiError::new(
                codes::NOT_FOUND,
                match requested.as_deref() {
                    Some(value) => format!(
                        "Console `{value}` is not among the advertised display outputs."
                    ),
                    None => "The requested console is not advertised by this node.".to_owned(),
                },
            )
            .with_details(serde_json::json!({
                "requested": requested,
                "available": available,
            }))
            .with_remediation(
                "Use a console index or label from GET /api/v1/connection.display_outputs.",
            ),
            Self::SingleHead {
                provider,
                active,
                requested,
            } => ApiError::new(
                codes::CONFLICT,
                format!(
                    "the {provider} capture provider exposes one default console and cannot \
                     capture Console_{requested} independently"
                ),
            )
            .with_details(serde_json::json!({
                "provider": provider,
                "active_console": active,
                "requested_console": requested,
                "single_head": true,
            }))
            .with_remediation(format!(
                "Use the active Console_{active} snapshot, or configure \
                 --capture-provider dbus for per-console capture."
            )),
            Self::ConsoleUnsupported => ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                "This node has no VM console selector or multi-console snapshot broker.",
            )
            .with_remediation(
                "Configure --profile vm with --capture-provider dbus and advertise consoles \
                 with --display-output before requesting ?console=.",
            ),
        }
    }
}

#[derive(Clone, Debug)]
struct CachedCapture {
    source_id: String,
    source_name: String,
    format: FrameFormat,
}

#[derive(Clone, Debug, Default)]
struct SnapshotCache {
    last: Option<CachedCapture>,
    last_error: Option<String>,
}

/// A capture source held open by an opt-in cached snapshot control.
///
/// The broker uses this so one console keeps one D-Bus/VNC/QMP source open
/// between requests instead of reconnecting for every JPEG. The capability is
/// retained with the source so frame metadata still describes the selected
/// console after the source is cached.
struct OpenSource {
    capability: CaptureCapability,
    source: Box<dyn CaptureSource>,
}

/// Where a snapshot control reads its capture registry.
///
/// The runtime console switch swaps the registry atomically; a shared slot lets
/// one-shot snapshots follow the same active `Console_N` as HLS and WebRTC
/// without rebuilding the compatibility state.
#[derive(Clone)]
enum SnapshotRegistry {
    Owned(Arc<CaptureRegistry>),
    Shared(Arc<std::sync::RwLock<CaptureRegistry>>),
}

impl SnapshotRegistry {
    fn current(&self) -> Arc<CaptureRegistry> {
        match self {
            Self::Owned(registry) => registry.clone(),
            Self::Shared(slot) => Arc::new(
                slot.read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            ),
        }
    }
}

/// Shared, cloneable snapshot control backed by a capture registry.
#[derive(Clone)]
pub struct SnapshotControl {
    registry: SnapshotRegistry,
    requested_source: Option<String>,
    timeout: Duration,
    quality: u8,
    cache: Arc<Mutex<SnapshotCache>>,
    /// Opt-in source caching for the multi-console broker.
    cache_source: bool,
    open: Arc<tokio::sync::Mutex<Option<OpenSource>>>,
}

impl SnapshotControl {
    /// Use the first source from `registry` (or `requested_source`) for snapshots.
    pub fn new(registry: CaptureRegistry) -> Self {
        Self {
            registry: SnapshotRegistry::Owned(Arc::new(registry)),
            requested_source: None,
            timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            quality: DEFAULT_JPEG_QUALITY,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
            cache_source: false,
            open: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Follow a registry slot that a runtime console switch can replace.
    pub fn from_shared(slot: Arc<std::sync::RwLock<CaptureRegistry>>) -> Self {
        Self {
            registry: SnapshotRegistry::Shared(slot),
            requested_source: None,
            timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            quality: DEFAULT_JPEG_QUALITY,
            cache: Arc::new(Mutex::new(SnapshotCache::default())),
            cache_source: false,
            open: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Keep the opened capture source between snapshot calls.
    ///
    /// The default is one-shot: open, read one frame, stop. The bounded console
    /// broker opts in so `GET /display/snapshot?console=N` does not reconnect to
    /// QEMU for every preview poll. `shutdown()` stops a cached source.
    pub fn with_source_cache(mut self, enabled: bool) -> Self {
        self.cache_source = enabled;
        self
    }

    /// True when this control keeps an opened source between calls.
    pub fn caches_source(&self) -> bool {
        self.cache_source
    }

    /// Stop a cached source, if any. Idempotent.
    pub async fn shutdown(&self) {
        let mut open = self.open.lock().await;
        if let Some(mut cached) = open.take() {
            let _ = cached.source.stop().await;
        }
    }

    /// Open the selected capture source for a long-lived encoded stream.
    ///
    /// This follows the same registry/source selection as [`Self::capture`]
    /// but returns the source itself instead of a JPEG, so `/video?codec=h264`
    /// can feed the original layout (including compressed H.264/HEVC access
    /// units) to ffmpeg. The caller owns the source and must call
    /// [`CaptureSource::stop`] when the stream ends.
    pub async fn open_stream_source(
        &self,
    ) -> Result<(CaptureCapability, Box<dyn CaptureSource>), SnapshotError> {
        self.open_source().await
    }

    /// Prefer a specific source id instead of the first enumerated source.
    pub fn with_source_id(mut self, id: impl Into<String>) -> Self {
        self.requested_source = Some(id.into());
        self
    }

    /// Bound source discovery, opening, one frame, and encoding.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// JPEG quality, clamped to `1..=100` by the encoder helper.
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality;
        self
    }

    /// Build an independent long-lived control for one MJPEG stream.
    ///
    /// The clone follows the same registry (including a runtime console switch)
    /// and shares the last-frame cache, but gets its own source slot: it caches
    /// the opened source between [`Self::capture`] calls, so `/video` reads
    /// frames from one provider connection instead of reopening the device for
    /// every JPEG. Each stream owns a separate slot, so closing one stream never
    /// stops a concurrent snapshot or another stream. Call [`Self::shutdown`]
    /// when the stream ends.
    pub fn streaming(&self, quality: Option<u8>) -> Self {
        Self {
            registry: self.registry.clone(),
            requested_source: self.requested_source.clone(),
            timeout: self.timeout,
            quality: quality.unwrap_or(self.quality),
            cache: self.cache.clone(),
            cache_source: true,
            open: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    fn cache(&self) -> MutexGuard<'_, SnapshotCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Report the configured source and the last successful capture, without
    /// opening the device.
    pub async fn info(&self) -> DisplayInfo {
        let registry = self.registry.current();
        let requested = self.requested_source.clone();
        let discovered = tokio::task::spawn_blocking(move || {
            let capabilities = registry.enumerate()?;
            let capability = select_capability(capabilities, requested.as_deref());
            Ok::<_, ApiError>(capability)
        })
        .await;

        let cached = self.cache().clone();
        match discovered {
            Ok(Ok(Some(capability))) => {
                if let Some(last) = cached.last {
                    DisplayInfo {
                        ok: true,
                        source_id: Some(last.source_id),
                        source_name: Some(last.source_name),
                        width: last.format.width,
                        height: last.format.height,
                        pixel_format: Some(last.format.pixel_format.as_str().to_owned()),
                        capability_max_width: Some(capability.max_width),
                        capability_max_height: Some(capability.max_height),
                        snapshot_available: true,
                        snapshot_encoding: "jpeg",
                        dimensions_source: "last-frame",
                        last_error: cached.last_error,
                        console: None,
                        active: false,
                    }
                } else {
                    DisplayInfo {
                        ok: true,
                        // No frame has been captured yet, so the current frame
                        // geometry is genuinely unknown. The capability limit
                        // is reported separately and must not masquerade as a
                        // real frame size.
                        width: 0,
                        height: 0,
                        pixel_format: capability.formats.first().cloned(),
                        capability_max_width: Some(capability.max_width),
                        capability_max_height: Some(capability.max_height),
                        source_id: Some(capability.id),
                        source_name: Some(capability.name),
                        snapshot_available: true,
                        snapshot_encoding: "jpeg",
                        dimensions_source: "unknown",
                        last_error: cached.last_error,
                        console: None,
                        active: false,
                    }
                }
            }
            Ok(Ok(None)) => DisplayInfo {
                ok: true,
                source_id: None,
                source_name: None,
                width: 0,
                height: 0,
                pixel_format: None,
                capability_max_width: None,
                capability_max_height: None,
                snapshot_available: false,
                snapshot_encoding: "jpeg",
                dimensions_source: "none",
                last_error: cached.last_error,
                console: None,
                active: false,
            },
            Ok(Err(error)) => DisplayInfo {
                ok: true,
                source_id: None,
                source_name: None,
                width: 0,
                height: 0,
                pixel_format: None,
                capability_max_width: None,
                capability_max_height: None,
                snapshot_available: false,
                snapshot_encoding: "jpeg",
                dimensions_source: "none",
                last_error: cached.last_error.or(Some(error.message)),
                console: None,
                active: false,
            },
            Err(error) => DisplayInfo {
                ok: true,
                source_id: None,
                source_name: None,
                width: 0,
                height: 0,
                pixel_format: None,
                capability_max_width: None,
                capability_max_height: None,
                snapshot_available: false,
                snapshot_encoding: "jpeg",
                dimensions_source: "none",
                last_error: cached.last_error.or(Some(error.to_string())),
                console: None,
                active: false,
            },
        }
    }

    /// Open the selected source, read exactly one frame, and encode it as JPEG.
    ///
    /// One-shot controls stop the source before returning. A control built with
    /// [`Self::with_source_cache`] keeps it open and reads the next frame from
    /// the same provider connection on each call.
    pub async fn capture(&self) -> Result<SnapshotFrame, SnapshotError> {
        let (capability, frame) = self.next_frame_from_source().await?;
        let format = frame.format();
        let quality = self.quality;
        let encoding = tokio::time::timeout(
            self.timeout,
            tokio::task::spawn_blocking(move || ozma_encode::encode_jpeg_frame(&frame, quality)),
        )
        .await;
        let jpeg = match encoding {
            Ok(Ok(result)) => result.map_err(SnapshotError::Encode)?,
            Ok(Err(error)) => return Err(SnapshotError::Task(error.to_string())),
            Err(_) => {
                let message = format!(
                    "display snapshot encoding did not finish within {:?}",
                    self.timeout
                );
                self.cache().last_error = Some(message);
                return Err(SnapshotError::EncodeTimeout {
                    timeout: self.timeout,
                });
            }
        };

        let cached = CachedCapture {
            source_id: capability.id.clone(),
            source_name: capability.name.clone(),
            format,
        };
        {
            let mut cache = self.cache();
            cache.last = Some(cached);
            cache.last_error = None;
        }

        Ok(SnapshotFrame {
            jpeg,
            source_id: capability.id,
            source_name: capability.name,
            format,
        })
    }

    async fn next_frame_from_source(&self) -> Result<(CaptureCapability, Frame), SnapshotError> {
        if !self.cache_source {
            let (capability, mut source) = self.open_source().await?;
            let source_id = capability.id.clone();
            let frame = match tokio::time::timeout(self.timeout, source.next_frame()).await {
                Ok(Ok(frame)) => frame,
                Ok(Err(error)) => {
                    let _ = source.stop().await;
                    let message = format!("{error}");
                    self.cache().last_error = Some(message);
                    return Err(SnapshotError::Capture { source_id, error });
                }
                Err(_) => {
                    let _ = source.stop().await;
                    let message = format!(
                        "capture source `{source_id}` did not produce a frame within {:?}",
                        self.timeout
                    );
                    self.cache().last_error = Some(message);
                    return Err(SnapshotError::Timeout {
                        source_id,
                        timeout: self.timeout,
                    });
                }
            };
            let _ = source.stop().await;
            return Ok((capability, frame));
        }

        let mut open = self.open.lock().await;
        if open.is_none() {
            let (capability, source) = self.open_source().await?;
            *open = Some(OpenSource { capability, source });
        }
        let cached = open.as_mut().expect("cached source was just installed");
        let source_id = cached.capability.id.clone();
        match tokio::time::timeout(self.timeout, cached.source.next_frame()).await {
            Ok(Ok(frame)) => Ok((cached.capability.clone(), frame)),
            Ok(Err(error)) => {
                let message = format!("{error}");
                let mut failed = open.take().expect("cached source");
                let _ = failed.source.stop().await;
                drop(open);
                self.cache().last_error = Some(message);
                Err(SnapshotError::Capture { source_id, error })
            }
            Err(_) => {
                let mut failed = open.take().expect("cached source");
                let _ = failed.source.stop().await;
                drop(open);
                let message = format!(
                    "capture source `{source_id}` did not produce a frame within {:?}",
                    self.timeout
                );
                self.cache().last_error = Some(message);
                Err(SnapshotError::Timeout {
                    source_id,
                    timeout: self.timeout,
                })
            }
        }
    }

    async fn open_source(
        &self,
    ) -> Result<(CaptureCapability, Box<dyn CaptureSource>), SnapshotError> {
        let registry = self.registry.current();
        let requested = self.requested_source.clone();
        let task = tokio::task::spawn_blocking(move || {
            let capabilities = registry.enumerate().map_err(SnapshotError::Open)?;
            let capability =
                select_capability(capabilities, requested.as_deref()).ok_or_else(|| {
                    SnapshotError::NoSource {
                        requested: requested.clone(),
                    }
                })?;
            let id = capability.id.clone();
            let source = registry.open(&capability).map_err(SnapshotError::Open)?;
            if source.id() != id {
                // Providers are allowed to report a different concrete id, but
                // the frame metadata must still describe the selected source.
                tracing::debug!(
                    selected = %id,
                    opened = source.id(),
                    "snapshot provider opened a differently named source"
                );
            }
            Ok::<_, SnapshotError>((capability, source))
        });

        match tokio::time::timeout(self.timeout, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(SnapshotError::Task(error.to_string())),
            Err(_) => Err(SnapshotError::Timeout {
                source_id: self.requested_source.clone().unwrap_or_default(),
                timeout: self.timeout,
            }),
        }
    }
}

fn select_capability(
    capabilities: Vec<CaptureCapability>,
    requested: Option<&str>,
) -> Option<CaptureCapability> {
    match requested {
        Some(id) => capabilities
            .into_iter()
            .find(|capability| capability.id == id),
        None => capabilities.into_iter().next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ozma_capture::FakeCaptureProvider;
    use ozma_media::{Frame, PixelFormat};
    use ozma_protocol::CaptureKind;
    use std::io::Cursor;

    fn fake_registry() -> CaptureRegistry {
        CaptureRegistry::new().with(FakeCaptureProvider::new())
    }

    fn jpeg_dimensions(jpeg: &[u8]) -> (u16, u16) {
        let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(jpeg));
        let pixels = decoder.decode().expect("valid JPEG");
        assert!(!pixels.is_empty());
        let info = decoder.info().expect("JPEG info");
        (info.width, info.height)
    }

    #[tokio::test]
    async fn fake_registry_snapshot_is_a_real_jpeg_with_expected_dimensions() {
        let control = SnapshotControl::new(fake_registry());
        let snapshot = control.capture().await.expect("snapshot");
        assert!(snapshot.jpeg.starts_with(&[0xFF, 0xD8]));
        assert_eq!(jpeg_dimensions(&snapshot.jpeg), (320, 240));
        assert_eq!(snapshot.source_id, "fake0");
        assert_eq!(snapshot.format, FrameFormat::rgb24(320, 240));

        let info = control.info().await;
        assert!(info.ok);
        assert!(info.snapshot_available);
        assert_eq!(info.source_id.as_deref(), Some("fake0"));
        assert_eq!((info.width, info.height), (320, 240));
        assert_eq!(info.pixel_format.as_deref(), Some("rgb24"));
        assert_eq!(info.dimensions_source, "last-frame");
        assert!(info.last_error.is_none());
    }

    #[tokio::test]
    async fn info_before_the_first_frame_does_not_report_capability_max_as_frame_size() {
        let control = SnapshotControl::new(fake_registry());

        let info = control.info().await;
        assert!(info.ok);
        assert!(info.snapshot_available);
        assert_eq!(
            (info.width, info.height),
            (0, 0),
            "current frame geometry is unknown before the first frame"
        );
        assert_eq!(info.dimensions_source, "unknown");
        assert_eq!(info.capability_max_width, Some(320));
        assert_eq!(info.capability_max_height, Some(240));
        assert_eq!(info.source_id.as_deref(), Some("fake0"));

        let snapshot = control.capture().await.expect("snapshot");
        assert_eq!((snapshot.format.width, snapshot.format.height), (320, 240));

        let info = control.info().await;
        assert_eq!((info.width, info.height), (320, 240));
        assert_eq!(info.dimensions_source, "last-frame");
        assert_eq!(info.capability_max_width, Some(320));
        assert_eq!(info.capability_max_height, Some(240));
    }

    fn multi_source_registry() -> CaptureRegistry {
        CaptureRegistry::new()
            .with(
                FakeCaptureProvider::new()
                    .with_identity("/dev/video0", "Creative Live! Cam")
                    .with_format(FrameFormat::rgb24(64, 48)),
            )
            .with(
                FakeCaptureProvider::new()
                    .with_identity("/dev/video42", "ozma-capture-test")
                    .with_format(FrameFormat::rgb24(32, 24)),
            )
    }

    #[tokio::test]
    async fn snapshot_uses_the_selected_capture_device_not_the_first_camera() {
        let control = SnapshotControl::new(multi_source_registry()).with_source_id("/dev/video42");
        let snapshot = control.capture().await.expect("selected snapshot");
        assert_eq!(snapshot.source_id, "/dev/video42");
        assert_eq!((snapshot.format.width, snapshot.format.height), (32, 24));
        assert_eq!(snapshot.format.pixel_format, PixelFormat::Rgb24);

        let info = control.info().await;
        assert_eq!(info.source_id.as_deref(), Some("/dev/video42"));
    }

    #[tokio::test]
    async fn snapshot_missing_selected_device_is_typed_and_does_not_fall_back() {
        let control = SnapshotControl::new(multi_source_registry()).with_source_id("/dev/video99");
        let error = control.capture().await.expect_err("missing device");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        let api = error.api_error();
        assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
        assert!(api.message.contains("/dev/video99"), "{}", api.message);
    }

    #[tokio::test]
    async fn bgra_source_is_encoded() {
        let registry = CaptureRegistry::new().with(
            FakeCaptureProvider::new().with_format(FrameFormat::new(PixelFormat::Bgra, 16, 8)),
        );
        let snapshot = SnapshotControl::new(registry)
            .capture()
            .await
            .expect("snapshot");
        assert_eq!(jpeg_dimensions(&snapshot.jpeg), (16, 8));
    }

    #[tokio::test]
    async fn yuyv_and_nv12_sources_are_encoded() {
        for format in [
            FrameFormat::new(PixelFormat::Yuyv, 16, 8),
            FrameFormat::new(PixelFormat::Nv12, 16, 8),
        ] {
            let registry =
                CaptureRegistry::new().with(FakeCaptureProvider::new().with_format(format));
            let snapshot = SnapshotControl::new(registry)
                .capture()
                .await
                .unwrap_or_else(|error| panic!("{format:?} snapshot failed: {error}"));
            assert_eq!(snapshot.format, format);
            assert!(snapshot.jpeg.starts_with(&[0xFF, 0xD8]));
            assert_eq!(jpeg_dimensions(&snapshot.jpeg), (16, 8));
        }
    }

    #[tokio::test]
    async fn no_source_is_a_typed_unavailable_error() {
        let control = SnapshotControl::new(CaptureRegistry::new());
        let error = control.capture().await.unwrap_err();
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        let api = error.api_error();
        assert_eq!(api.code, codes::CAPTURE_UNAVAILABLE);
        assert!(api.remediation.is_some());

        let info = control.info().await;
        assert!(info.ok);
        assert!(!info.snapshot_available);
        assert_eq!(info.dimensions_source, "none");
    }

    #[tokio::test]
    async fn missing_requested_source_is_typed() {
        let control = SnapshotControl::new(fake_registry()).with_source_id("qmp9");
        let error = control.capture().await.unwrap_err();
        assert!(matches!(
            error,
            SnapshotError::NoSource { ref requested } if requested.as_deref() == Some("qmp9")
        ));
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn unsupported_frame_format_is_typed() {
        let format = FrameFormat::new(PixelFormat::H264, 4, 4);
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new().with_format(format));
        let error = SnapshotControl::new(registry).capture().await.unwrap_err();
        assert_eq!(
            error,
            SnapshotError::Encode(ozma_encode::JpegError::UnsupportedFormat(PixelFormat::H264))
        );
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        let api = error.api_error();
        assert_eq!(api.code, "display.unsupported_format");
        assert!(api.remediation.unwrap().contains("yuyv"));
    }

    #[tokio::test]
    async fn a_slow_source_times_out_with_remediation() {
        let registry = CaptureRegistry::new().with(SlowProvider);
        let control = SnapshotControl::new(registry).with_timeout(Duration::from_millis(20));
        let error = control.capture().await.unwrap_err();
        assert!(matches!(error, SnapshotError::Timeout { .. }));
        assert_eq!(error.status(), StatusCode::GATEWAY_TIMEOUT);
        let api = error.api_error();
        assert_eq!(api.code, "display.snapshot_timeout");
        assert!(api.remediation.is_some());
    }

    #[tokio::test]
    async fn mjpeg_source_is_passed_through_unchanged() {
        let jpeg = ozma_encode::encode_jpeg_rgb(&[200, 30, 40, 10, 220, 90], 2, 1, 95).unwrap();
        let registry = CaptureRegistry::new().with(StaticFrameProvider {
            id: "mjpeg0".to_owned(),
            format: FrameFormat::new(PixelFormat::Mjpeg, 2, 1),
            payload: jpeg.clone(),
        });
        let control = SnapshotControl::new(registry);
        let snapshot = control.capture().await.expect("MJPEG snapshot");
        assert_eq!(snapshot.jpeg, jpeg, "MJPEG must be passed through");
        assert_eq!(snapshot.format.pixel_format, PixelFormat::Mjpeg);
        let info = control.info().await;
        assert_eq!(info.pixel_format.as_deref(), Some("mjpeg"));
        assert_eq!((info.width, info.height), (2, 1));
    }

    struct StaticFrameProvider {
        id: String,
        format: FrameFormat,
        payload: Vec<u8>,
    }

    impl ozma_capture::CaptureProvider for StaticFrameProvider {
        fn kind(&self) -> CaptureKind {
            CaptureKind::Fake
        }

        fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
            Ok(vec![CaptureCapability {
                id: self.id.clone(),
                name: "Static test frame".to_owned(),
                kind: CaptureKind::Fake,
                formats: vec![self.format.pixel_format.as_str().to_owned()],
                max_width: self.format.width,
                max_height: self.format.height,
                max_fps: 1,
                compressed: self.format.pixel_format.is_compressed(),
                hdr: false,
                has_audio: false,
            }])
        }

        fn open(
            &self,
            _capability: &CaptureCapability,
        ) -> Result<Box<dyn CaptureSource>, ApiError> {
            Ok(Box::new(StaticFrameSource {
                id: self.id.clone(),
                format: self.format,
                payload: self.payload.clone(),
            }))
        }
    }

    struct StaticFrameSource {
        id: String,
        format: FrameFormat,
        payload: Vec<u8>,
    }

    #[async_trait]
    impl CaptureSource for StaticFrameSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            Frame::with_data(self.format, 0, 0, true, self.payload.clone())
        }
    }

    struct SlowProvider;

    impl ozma_capture::CaptureProvider for SlowProvider {
        fn kind(&self) -> CaptureKind {
            CaptureKind::Fake
        }

        fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
            Ok(vec![CaptureCapability {
                id: "slow0".to_owned(),
                name: "Slow fake".to_owned(),
                kind: CaptureKind::Fake,
                formats: vec![PixelFormat::Rgb24.as_str().to_owned()],
                max_width: 2,
                max_height: 2,
                max_fps: 1,
                compressed: false,
                hdr: false,
                has_audio: false,
            }])
        }

        fn open(
            &self,
            _capability: &CaptureCapability,
        ) -> Result<Box<dyn CaptureSource>, ApiError> {
            Ok(Box::new(SlowSource {
                id: "slow0".to_owned(),
            }))
        }
    }

    struct SlowSource {
        id: String,
    }

    #[async_trait]
    impl CaptureSource for SlowSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn format(&self) -> FrameFormat {
            FrameFormat::rgb24(2, 2)
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Frame::with_data(FrameFormat::rgb24(2, 2), 0, 0, true, vec![0u8; 12])
        }
    }
}
