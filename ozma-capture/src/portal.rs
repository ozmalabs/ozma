// SPDX-License-Identifier: AGPL-3.0-only
//! XDG Desktop Portal `org.freedesktop.portal.ScreenCast` client and capture
//! adapter.
//!
//! A desktop session normally grants screen capture through the portal, not by
//! enumerating the PipeWire graph directly. The portal is a permission and
//! selection front-end:
//!
//! ```text
//! CreateSession -> SelectSources(monitor|window) -> Start -> streams[node_id]
//!               -> OpenPipeWireRemote -> PipeWire fd
//! ```
//!
//! After consent, the selected monitor/window is a normal PipeWire
//! `Video/Source` node. [`PortalScreenCast`] performs the bounded D-Bus
//! handshake and returns a [`PortalStream`] containing the node id and, when the
//! portal supplies one, the `OpenPipeWireRemote` fd.
//! [`PortalCaptureProvider`] then hands that node to the existing
//! [`PipeWireCaptureProvider`](crate::pipewire::PipeWireCaptureProvider) path as
//! `pipewire:node:<id>`.
//!
//! ## Why the fd is held instead of connected
//!
//! The portal's fd is the permission-bearing PipeWire remote connection. This
//! crate's PipeWire backend has no fd-connect API (it connects to the session
//! daemon by name), so the adapter cannot honestly pass the fd into that path.
//! It therefore:
//!
//! * keeps the fd (and the portal D-Bus connection/session) alive for the
//!   lifetime of the capture source (so the portal session/permission is not
//!   revoked while frames are read), and
//! * opens the returned node id through the normal session PipeWire
//!   connection.
//!
//! On compositors that only permit the portal fd this open fails with the
//! underlying typed PipeWire error; it is never reported as a successful
//! screen-cast. [`PortalCaptureConfig::require_pipewire_fd`] defaults to `true`
//! so a portal response without a remote fd is a typed `unsupported` capability
//! rather than a silent node-id-only claim.
//!
//! ## Testability and scope
//!
//! The D-Bus operations are behind [`PortalTransport`]. Tests inject a scripted
//! transport to assert request/response parsing, denial/cancellation, timeouts,
//! a missing bus/service, and fd/node handling. The production transport uses
//! `zbus` (Linux-only, optional `portal` feature). The real portal test is
//! `#[ignore]`d and gated by `OZMA_PORTAL_TEST=1`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use zbus::export::futures_util::StreamExt;
use zbus::zvariant::{Array, Dict, ObjectPath, OwnedObjectPath, OwnedValue, Structure, Value};

use crate::pipewire::{PipeWireCaptureProvider, PipeWireOptions};
use crate::provider::CaptureProvider;
use ozma_pipewire::{StreamTarget, VideoPixelFormat};

/// Well-known bus name of the desktop portal.
pub const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";

/// Object path of the portal's ScreenCast interface.
pub const PORTAL_OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// ScreenCast interface name.
pub const SCREENCAST_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";

/// Request object interface.
pub const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

/// Session object interface.
pub const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";

/// Minimum ScreenCast version this client accepts.
pub const MIN_SCREENCAST_VERSION: u32 = 1;

/// ScreenCast version that first exported virtual sources/restore tokens.
pub const SCREENCAST_VERSION_VIRTUAL: u32 = 2;

/// Default bound on one portal request, including the user's consent dialog.
pub const PORTAL_DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(180);

/// Default bound on the availability probe (bus, service name, version).
pub const PORTAL_DEFAULT_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(2);

/// Default provider source id.
pub const PORTAL_DEFAULT_ID: &str = "portal0";

/// Default provider source name.
pub const PORTAL_DEFAULT_NAME: &str = "Desktop portal screen cast";

/// Default advertised width.
pub const PORTAL_DEFAULT_WIDTH: u32 = 1920;

/// Default advertised height.
pub const PORTAL_DEFAULT_HEIGHT: u32 = 1080;

/// Default advertised frame rate.
pub const PORTAL_DEFAULT_FPS: u32 = 30;

/// Largest width or height [`PortalCaptureConfig`] accepts.
pub const MAX_PORTAL_DIMENSION: u32 = 16_384;

/// Portal source selection kind (`SelectSources.types`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortalSourceType {
    /// Select a monitor (the desktop screen).
    #[default]
    Monitor,
    /// Select a single window.
    Window,
    /// Select a virtual source (ScreenCast version >= 2).
    Virtual,
}

impl PortalSourceType {
    /// D-Bus bit value for `SelectSources`.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Monitor => 1,
            Self::Window => 2,
            Self::Virtual => 4,
        }
    }

    /// Lowest ScreenCast version that supports this source type.
    pub fn minimum_version(self) -> u32 {
        match self {
            Self::Monitor | Self::Window => MIN_SCREENCAST_VERSION,
            Self::Virtual => SCREENCAST_VERSION_VIRTUAL,
        }
    }
}

/// Cursor mode requested from the portal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PortalCursorMode {
    /// Do not include the cursor.
    #[default]
    Hidden,
    /// Draw the cursor into the stream.
    Embedded,
    /// Include cursor metadata separately.
    Metadata,
}

impl PortalCursorMode {
    fn as_u32(self) -> u32 {
        match self {
            Self::Hidden => 1,
            Self::Embedded => 2,
            Self::Metadata => 4,
        }
    }
}

/// Configuration for [`PortalScreenCast`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalConfig {
    /// Which source type the selection dialog offers.
    pub source: PortalSourceType,
    /// Allow selecting more than one source.
    pub multiple: bool,
    /// Cursor handling for the stream.
    pub cursor_mode: PortalCursorMode,
    /// Optional restore token from an earlier session.
    pub restore_token: Option<String>,
    /// Bound on one portal request, including consent.
    pub response_timeout: Duration,
    /// Bound on the availability probe.
    pub preflight_timeout: Duration,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            source: PortalSourceType::Monitor,
            multiple: false,
            cursor_mode: PortalCursorMode::Hidden,
            restore_token: None,
            response_timeout: PORTAL_DEFAULT_RESPONSE_TIMEOUT,
            preflight_timeout: PORTAL_DEFAULT_PREFLIGHT_TIMEOUT,
        }
    }
}

impl PortalConfig {
    /// Override the source type.
    pub fn with_source(mut self, source: PortalSourceType) -> Self {
        self.source = source;
        self
    }

    /// Allow or forbid multiple selected sources.
    pub fn with_multiple(mut self, multiple: bool) -> Self {
        self.multiple = multiple;
        self
    }

    /// Override cursor handling.
    pub fn with_cursor_mode(mut self, cursor_mode: PortalCursorMode) -> Self {
        self.cursor_mode = cursor_mode;
        self
    }

    /// Reuse a restore token from an earlier portal session.
    pub fn with_restore_token(mut self, token: impl Into<String>) -> Self {
        self.restore_token = Some(token.into());
        self
    }

    /// Override the per-request/consent timeout.
    pub fn with_response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = timeout;
        self
    }

    /// Override the availability probe timeout.
    pub fn with_preflight_timeout(mut self, timeout: Duration) -> Self {
        self.preflight_timeout = timeout;
        self
    }

    fn validate(&self) -> Result<(), PortalError> {
        if self.response_timeout.is_zero() {
            return Err(PortalError::InvalidConfig(
                "the portal response timeout must be greater than zero".to_owned(),
            ));
        }
        if self.preflight_timeout.is_zero() {
            return Err(PortalError::InvalidConfig(
                "the portal preflight timeout must be greater than zero".to_owned(),
            ));
        }
        if let Some(token) = &self.restore_token {
            if token.is_empty() || token.chars().any(|character| character.is_control()) {
                return Err(PortalError::InvalidConfig(
                    "the portal restore token must be non-empty and contain no control characters"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Result of probing the portal service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalPreflight {
    /// `DBUS_SESSION_BUS_ADDRESS` as seen by this process, if set.
    pub bus_address: Option<String>,
    /// Unique bus owner of [`PORTAL_BUS_NAME`].
    pub owner: String,
    /// Advertised `org.freedesktop.portal.ScreenCast` version.
    pub screencast_version: u32,
}

/// One selected stream from the portal's `Start` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalStreamInfo {
    /// PipeWire node id to open.
    pub node_id: u32,
    /// Position of the stream within the desktop, when reported.
    pub position: Option<(i32, i32)>,
    /// Size of the stream in pixels, when reported.
    pub size: Option<(i32, i32)>,
    /// Portal source type (1 monitor, 2 window, 4 virtual).
    pub source_type: Option<u32>,
    /// Mapping id for virtual sources, when reported.
    pub mapping_id: Option<String>,
}

/// A successful portal ScreenCast `Start` plus the optional PipeWire fd.
pub struct PortalStream {
    /// Object path of the portal session, for diagnostics/close.
    pub session_handle: String,
    /// Streams selected by the user.
    pub streams: Vec<PortalStreamInfo>,
    /// `OpenPipeWireRemote` fd, when the portal supplied one.
    pub pipewire_fd: Option<OwnedFd>,
    /// Why the fd is absent, when the call failed but the node id is known.
    pub pipewire_fd_error: Option<String>,
    /// Restore token for a later session, when reported.
    pub restore_token: Option<String>,
    /// Keeps the portal D-Bus connection/session alive for the provider.
    ///
    /// [`PortalScreenCast::request_stream`] fills this; manual test
    /// construction may leave it `None`.
    #[doc(hidden)]
    pub keepalive: Option<Arc<dyn PortalTransport>>,
}

impl fmt::Debug for PortalStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalStream")
            .field("session_handle", &self.session_handle)
            .field("streams", &self.streams)
            .field(
                "pipewire_fd",
                &self.pipewire_fd.as_ref().map(|fd| fd.as_raw_fd()),
            )
            .field("pipewire_fd_error", &self.pipewire_fd_error)
            .field("restore_token", &self.restore_token)
            .finish()
    }
}

impl PortalStream {
    /// The first selected PipeWire node id.
    pub fn node_id(&self) -> Result<u32, PortalError> {
        self.streams
            .first()
            .map(|stream| stream.node_id)
            .ok_or_else(|| {
                PortalError::MissingResult(
                    "the portal returned no streams for the selected source".to_owned(),
                )
            })
    }

    /// PipeWire target for the first selected node.
    pub fn target(&self) -> Result<StreamTarget, PortalError> {
        self.node_id().map(StreamTarget::NodeId)
    }
}

/// Typed portal failures. None of these imply that capture succeeded.
#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    /// No D-Bus session bus is reachable.
    #[error("no D-Bus session bus is available: {message}")]
    BusUnavailable {
        /// Underlying zbus/connect error.
        message: String,
    },

    /// The session bus is reachable but the desktop portal is not running.
    #[error("the XDG Desktop Portal ScreenCast service is not running on the session bus")]
    ServiceUnavailable,

    /// The portal is too old for the requested operation.
    #[error(
        "the ScreenCast portal version is {found}, but {required} is required for this request"
    )]
    VersionTooOld {
        /// Advertised version.
        found: u32,
        /// Required version.
        required: u32,
    },

    /// The user denied or cancelled the consent/selection dialog.
    #[error("the portal request `{stage}` was denied or cancelled by the user")]
    Denied {
        /// D-Bus method whose request was denied.
        stage: String,
    },

    /// The portal returned a non-success `Response` code.
    #[error("the portal request `{stage}` failed with response code {response}{detail}")]
    RequestFailed {
        /// D-Bus method whose request failed.
        stage: String,
        /// Portal response code (normally 2).
        response: u32,
        /// Optional human-readable detail from the results.
        detail: String,
    },

    /// The portal request exceeded its bound.
    #[error("the portal request `{stage}` timed out after {timeout:?}")]
    Timeout {
        /// D-Bus method/stage that timed out.
        stage: String,
        /// Applied bound.
        timeout: Duration,
    },

    /// A response was missing a required field.
    #[error("the portal response is missing {0}")]
    MissingResult(String),

    /// A response field had the wrong type or shape.
    #[error("the portal response field `{field}` is malformed: {message}")]
    MalformedResult {
        /// Field name.
        field: String,
        /// What was expected/received.
        message: String,
    },

    /// The portal session exists but the fd cannot be obtained/represented.
    #[error("the portal did not provide a PipeWire remote fd: {message}")]
    PipeWireFdUnavailable {
        /// Underlying D-Bus/transport reason.
        message: String,
    },

    /// A D-Bus method call failed before a portal response was available.
    #[error("D-Bus call `{method}` failed: {message}")]
    Bus {
        /// D-Bus method that failed.
        method: String,
        /// Underlying error.
        message: String,
    },

    /// Configuration that cannot be honoured.
    #[error("invalid portal configuration: {0}")]
    InvalidConfig(String),
}

impl PortalError {
    /// Convert this failure into the node's typed API error shape.
    pub fn to_api_error(&self) -> ApiError {
        let (code, retryable) = match self {
            Self::Denied { .. } => (codes::CONSENT_DENIED, false),
            Self::VersionTooOld { .. } | Self::PipeWireFdUnavailable { .. } => {
                (codes::UNSUPPORTED, false)
            }
            Self::InvalidConfig(_) | Self::MalformedResult { .. } => {
                (codes::INVALID_REQUEST, false)
            }
            Self::MissingResult(_) => (codes::CAPTURE_UNAVAILABLE, true),
            Self::BusUnavailable { .. } | Self::ServiceUnavailable | Self::Bus { .. } => {
                (codes::CAPTURE_UNAVAILABLE, true)
            }
            Self::Timeout { .. } => (codes::CAPTURE_UNAVAILABLE, true),
            Self::RequestFailed { .. } => (codes::CAPTURE_UNAVAILABLE, true),
        };
        let api = ApiError::new(code, self.to_string());
        if retryable {
            api.retryable()
        } else {
            api
        }
    }
}

impl From<PortalError> for ApiError {
    fn from(error: PortalError) -> Self {
        error.to_api_error()
    }
}

/// Parsed `Response` results (`a{sv}`) from one portal request.
#[derive(Debug, Default)]
pub struct PortalResults {
    entries: BTreeMap<String, OwnedValue>,
}

impl PortalResults {
    fn from_response(entries: HashMap<String, OwnedValue>) -> Self {
        Self {
            entries: entries.into_iter().collect(),
        }
    }

    /// The session object path returned by `CreateSession`.
    pub fn session_handle(&self) -> Result<String, PortalError> {
        let value = self.entries.get("session_handle").ok_or_else(|| {
            PortalError::MissingResult("`session_handle` from CreateSession".to_owned())
        })?;
        owned_string(value).ok_or_else(|| PortalError::MalformedResult {
            field: "session_handle".to_owned(),
            message: "expected a string or object path".to_owned(),
        })
    }

    /// The selected streams returned by `Start`.
    pub fn streams(&self) -> Result<Vec<PortalStreamInfo>, PortalError> {
        let value = self
            .entries
            .get("streams")
            .ok_or_else(|| PortalError::MissingResult("`streams` from Start".to_owned()))?;
        parse_streams(value)
    }

    /// The restore token returned by `Start`, when present.
    pub fn restore_token(&self) -> Option<String> {
        self.entries.get("restore_token").and_then(owned_string)
    }

    /// A short human-readable dump for diagnostics, with no fd payload.
    pub fn detail(&self) -> String {
        self.entries
            .iter()
            .map(|(key, value)| format!("{key}={value:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn parse_streams(value: &OwnedValue) -> Result<Vec<PortalStreamInfo>, PortalError> {
    let array = <&Array<'_>>::try_from(value).map_err(|error| PortalError::MalformedResult {
        field: "streams".to_owned(),
        message: format!("expected a(ua{{sv}}): {error}"),
    })?;
    let mut streams = Vec::with_capacity(array.len());
    for element in array.inner() {
        let structure = value_structure(element).ok_or_else(|| PortalError::MalformedResult {
            field: "streams".to_owned(),
            message: format!("expected a stream tuple, got {element:?}"),
        })?;
        let fields = structure.fields();
        if fields.len() != 2 {
            return Err(PortalError::MalformedResult {
                field: "streams".to_owned(),
                message: format!("expected (u, a{{sv}}), got {} fields", fields.len()),
            });
        }
        let node_id = value_u32(&fields[0]).ok_or_else(|| PortalError::MalformedResult {
            field: "streams.id".to_owned(),
            message: format!("expected a non-negative integer, got {:?}", fields[0]),
        })?;
        let props = match value_dict(&fields[1]) {
            Some(dict) => dict,
            None => {
                return Err(PortalError::MalformedResult {
                    field: "streams.props".to_owned(),
                    message: format!("expected a{{sv}}, got {:?}", fields[1]),
                });
            }
        };

        let mut info = PortalStreamInfo {
            node_id,
            position: None,
            size: None,
            source_type: None,
            mapping_id: None,
        };
        for (key, value) in props.iter() {
            let Value::Str(key) = key else {
                continue;
            };
            match key.as_str() {
                "position" => info.position = value_pair(value),
                "size" => info.size = value_pair(value),
                "source_type" => info.source_type = value_u32(value),
                "mapping_id" => info.mapping_id = value_string(value),
                _ => {}
            }
        }
        streams.push(info);
    }
    Ok(streams)
}

fn value_structure<'a>(value: &'a Value<'a>) -> Option<&'a Structure<'a>> {
    match value {
        Value::Structure(structure) => Some(structure),
        Value::Value(inner) => value_structure(inner),
        _ => None,
    }
}

fn value_dict<'a>(value: &'a Value<'a>) -> Option<&'a Dict<'a, 'a>> {
    match value {
        Value::Dict(dict) => Some(dict),
        Value::Value(inner) => value_dict(inner),
        _ => None,
    }
}

fn value_u32(value: &Value<'_>) -> Option<u32> {
    match value {
        Value::U32(value) => Some(*value),
        Value::I32(value) if *value >= 0 => Some(*value as u32),
        Value::U64(value) => u32::try_from(*value).ok(),
        Value::I64(value) if *value >= 0 => u32::try_from(*value).ok(),
        Value::Value(inner) => value_u32(inner),
        _ => None,
    }
}

fn value_i32(value: &Value<'_>) -> Option<i32> {
    match value {
        Value::I32(value) => Some(*value),
        Value::U32(value) => i32::try_from(*value).ok(),
        Value::I64(value) => i32::try_from(*value).ok(),
        Value::U64(value) => i32::try_from(*value).ok(),
        Value::Value(inner) => value_i32(inner),
        _ => None,
    }
}

fn value_pair(value: &Value<'_>) -> Option<(i32, i32)> {
    let structure = match value {
        Value::Structure(structure) => structure,
        Value::Value(inner) => return value_pair(inner),
        _ => return None,
    };
    match structure.fields() {
        [first, second] => Some((value_i32(first)?, value_i32(second)?)),
        _ => None,
    }
}

fn value_string(value: &Value<'_>) -> Option<String> {
    match value {
        Value::Str(value) => Some(value.to_string()),
        Value::ObjectPath(value) => Some(value.to_string()),
        Value::Value(inner) => value_string(inner),
        _ => None,
    }
}

fn owned_string(value: &OwnedValue) -> Option<String> {
    if let Ok(text) = <&str>::try_from(value) {
        return Some(text.to_owned());
    }
    <&ObjectPath<'_>>::try_from(value)
        .ok()
        .map(|path| path.to_string())
}

/// Transport abstraction for the portal D-Bus surface.
///
/// Production uses [`ZbusPortalTransport`]; tests inject a scripted transport
/// so no session bus, consent dialog, or compositor is needed.
#[async_trait]
pub trait PortalTransport: Send + Sync {
    /// Return the unique name owning `name`, or `None` when it is not running.
    async fn name_owner(&self, name: &str) -> Result<Option<String>, PortalError>;

    /// Read the ScreenCast interface version.
    async fn screencast_version(&self) -> Result<u32, PortalError>;

    /// `CreateSession`, bounded by the configured response timeout.
    async fn create_session(
        &self,
        config: &PortalConfig,
        handle_token: &str,
        session_handle_token: &str,
    ) -> Result<PortalResults, PortalError>;

    /// `SelectSources` for the already-created session.
    async fn select_sources(
        &self,
        session: &str,
        config: &PortalConfig,
        handle_token: &str,
    ) -> Result<PortalResults, PortalError>;

    /// `Start` the selected streams.
    async fn start(
        &self,
        session: &str,
        config: &PortalConfig,
        handle_token: &str,
    ) -> Result<PortalResults, PortalError>;

    /// `OpenPipeWireRemote`; returns the permission-bearing fd.
    async fn open_pipewire_remote(&self, session: &str) -> Result<OwnedFd, PortalError>;

    /// Best-effort `Session.Close`.
    async fn close_session(&self, session: &str);
}

/// Production zbus transport for the session bus.
#[derive(Debug)]
pub struct ZbusPortalTransport {
    connection: zbus::Connection,
}

impl ZbusPortalTransport {
    /// Connect to the session bus.
    pub async fn connect() -> Result<Self, PortalError> {
        match zbus::Connection::session().await {
            Ok(connection) => Ok(Self { connection }),
            Err(error) => Err(PortalError::BusUnavailable {
                message: error.to_string(),
            }),
        }
    }

    async fn screencast_proxy(&self) -> Result<zbus::Proxy<'_>, PortalError> {
        zbus::Proxy::new(
            &self.connection,
            PORTAL_BUS_NAME,
            PORTAL_OBJECT_PATH,
            SCREENCAST_INTERFACE,
        )
        .await
        .map_err(|error| PortalError::Bus {
            method: "ScreenCast proxy".to_owned(),
            message: error.to_string(),
        })
    }

    async fn request_path(&self, handle_token: &str) -> Result<String, PortalError> {
        let unique = self
            .connection
            .unique_name()
            .ok_or_else(|| PortalError::Bus {
                method: "Request path".to_owned(),
                message: "the session bus connection has no unique name yet".to_owned(),
            })?;
        Ok(portal_request_path(unique.as_str(), handle_token))
    }

    async fn call_request(
        &self,
        method: &str,
        body: &(impl zbus::export::serde::Serialize + zbus::zvariant::DynamicType),
        request_path: &str,
        timeout: Duration,
    ) -> Result<PortalResults, PortalError> {
        let screencast = self.screencast_proxy().await?;
        let request = zbus::Proxy::new(
            &self.connection,
            PORTAL_BUS_NAME,
            request_path,
            REQUEST_INTERFACE,
        )
        .await
        .map_err(|error| PortalError::Bus {
            method: format!("{method} request proxy"),
            message: error.to_string(),
        })?;
        // Subscribe before calling: the portal may emit Response as soon as the
        // request exists, and losing that signal would turn a denial into a
        // timeout.
        let mut responses =
            Box::pin(request.receive_signal("Response").await.map_err(|error| {
                PortalError::Bus {
                    method: format!("{method} Response subscription"),
                    message: error.to_string(),
                }
            })?);

        let _reply: OwnedObjectPath =
            screencast
                .call(method, body)
                .await
                .map_err(|error| PortalError::Bus {
                    method: method.to_owned(),
                    message: error.to_string(),
                })?;

        let message = tokio::time::timeout(timeout, responses.next())
            .await
            .map_err(|_| PortalError::Timeout {
                stage: method.to_owned(),
                timeout,
            })?
            .ok_or_else(|| PortalError::Bus {
                method: method.to_owned(),
                message: "the portal Response signal stream closed".to_owned(),
            })?;

        let (response, entries): (u32, HashMap<String, OwnedValue>) = message
            .body()
            .deserialize()
            .map_err(|error| PortalError::MalformedResult {
                field: format!("{method}.Response"),
                message: error.to_string(),
            })?;

        response_to_results(method, response, entries)
    }
}

fn response_to_results(
    stage: &str,
    response: u32,
    entries: HashMap<String, OwnedValue>,
) -> Result<PortalResults, PortalError> {
    match response {
        0 => Ok(PortalResults::from_response(entries)),
        1 => Err(PortalError::Denied {
            stage: stage.to_owned(),
        }),
        response => Err(PortalError::RequestFailed {
            stage: stage.to_owned(),
            response,
            detail: PortalResults::from_response(entries).detail(),
        }),
    }
}

#[async_trait]
impl PortalTransport for ZbusPortalTransport {
    async fn name_owner(&self, name: &str) -> Result<Option<String>, PortalError> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await
        .map_err(|error| PortalError::Bus {
            method: "D-Bus proxy".to_owned(),
            message: error.to_string(),
        })?;
        let has_owner: bool = proxy
            .call("NameHasOwner", &(name,))
            .await
            .map_err(|error| PortalError::Bus {
                method: "NameHasOwner".to_owned(),
                message: error.to_string(),
            })?;
        if !has_owner {
            return Ok(None);
        }
        let owner: String = proxy
            .call("GetNameOwner", &(name,))
            .await
            .map_err(|error| PortalError::Bus {
                method: "GetNameOwner".to_owned(),
                message: error.to_string(),
            })?;
        Ok(Some(owner))
    }

    async fn screencast_version(&self) -> Result<u32, PortalError> {
        let proxy = self.screencast_proxy().await?;
        proxy
            .get_property::<u32>("version")
            .await
            .map_err(|error| PortalError::Bus {
                method: "ScreenCast.version".to_owned(),
                message: error.to_string(),
            })
    }

    async fn create_session(
        &self,
        config: &PortalConfig,
        handle_token: &str,
        session_handle_token: &str,
    ) -> Result<PortalResults, PortalError> {
        let mut options = HashMap::new();
        options.insert(
            "handle_token".to_owned(),
            Value::from(handle_token.to_owned()),
        );
        options.insert(
            "session_handle_token".to_owned(),
            Value::from(session_handle_token.to_owned()),
        );
        let request_path = self.request_path(handle_token).await?;
        self.call_request(
            "CreateSession",
            &(options,),
            &request_path,
            config.response_timeout,
        )
        .await
    }

    async fn select_sources(
        &self,
        session: &str,
        config: &PortalConfig,
        handle_token: &str,
    ) -> Result<PortalResults, PortalError> {
        let session_path =
            ObjectPath::try_from(session).map_err(|error| PortalError::MalformedResult {
                field: "session_handle".to_owned(),
                message: error.to_string(),
            })?;
        let mut options = HashMap::new();
        options.insert(
            "handle_token".to_owned(),
            Value::from(handle_token.to_owned()),
        );
        options.insert("types".to_owned(), Value::from(config.source.as_u32()));
        options.insert("multiple".to_owned(), Value::from(config.multiple));
        options.insert(
            "cursor_mode".to_owned(),
            Value::from(config.cursor_mode.as_u32()),
        );
        if let Some(token) = &config.restore_token {
            options.insert("restore_token".to_owned(), Value::from(token.clone()));
        }
        let request_path = self.request_path(handle_token).await?;
        self.call_request(
            "SelectSources",
            &(session_path, options),
            &request_path,
            config.response_timeout,
        )
        .await
    }

    async fn start(
        &self,
        session: &str,
        config: &PortalConfig,
        handle_token: &str,
    ) -> Result<PortalResults, PortalError> {
        let session_path =
            ObjectPath::try_from(session).map_err(|error| PortalError::MalformedResult {
                field: "session_handle".to_owned(),
                message: error.to_string(),
            })?;
        let mut options = HashMap::new();
        options.insert(
            "handle_token".to_owned(),
            Value::from(handle_token.to_owned()),
        );
        let request_path = self.request_path(handle_token).await?;
        self.call_request(
            "Start",
            &(session_path, String::new(), options),
            &request_path,
            config.response_timeout,
        )
        .await
    }

    async fn open_pipewire_remote(&self, session: &str) -> Result<OwnedFd, PortalError> {
        let proxy = self.screencast_proxy().await?;
        let session_path =
            ObjectPath::try_from(session).map_err(|error| PortalError::MalformedResult {
                field: "session_handle".to_owned(),
                message: error.to_string(),
            })?;
        let options: HashMap<String, Value<'static>> = HashMap::new();
        let fd: zbus::zvariant::OwnedFd = proxy
            .call("OpenPipeWireRemote", &(session_path, options))
            .await
            .map_err(|error| PortalError::Bus {
                method: "OpenPipeWireRemote".to_owned(),
                message: error.to_string(),
            })?;
        Ok(fd.into())
    }

    async fn close_session(&self, session: &str) {
        let Ok(proxy) = zbus::Proxy::new(
            &self.connection,
            PORTAL_BUS_NAME,
            session,
            SESSION_INTERFACE,
        )
        .await
        else {
            return;
        };
        let _ = proxy.call::<_, _, ()>("Close", &()).await;
    }
}

/// Compute the portal request object path for `handle_token`.
///
/// The portal derives the path from the caller's unique bus name and the token;
/// subscribing there before the method call avoids racing the Response signal.
pub fn portal_request_path(unique_name: &str, handle_token: &str) -> String {
    let sender = unique_name
        .trim_start_matches(':')
        .replace('.', "_")
        .replace('-', "_");
    format!("/org/freedesktop/portal/desktop/request/{sender}/{handle_token}")
}

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

fn request_token(prefix: &str) -> String {
    let counter = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("ozma_{prefix}_{pid}_{counter}")
}

/// A bounded ScreenCast client.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use ozma_capture::{PortalConfig, PortalScreenCast};
///
/// let portal = PortalScreenCast::connect(PortalConfig::default()).await?;
/// let stream = portal.request_stream().await?;
/// println!("selected node {}", stream.node_id()?);
/// # Ok(())
/// # }
/// ```
pub struct PortalScreenCast {
    config: PortalConfig,
    transport: Arc<dyn PortalTransport>,
}

impl fmt::Debug for PortalScreenCast {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalScreenCast")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PortalScreenCast {
    /// Connect to the session bus and build a client with the zbus transport.
    pub async fn connect(config: PortalConfig) -> Result<Self, PortalError> {
        config.validate()?;
        let connection =
            tokio::time::timeout(config.preflight_timeout, ZbusPortalTransport::connect())
                .await
                .map_err(|_| PortalError::Timeout {
                    stage: "session bus connect".to_owned(),
                    timeout: config.preflight_timeout,
                })??;
        Ok(Self {
            config,
            transport: Arc::new(connection),
        })
    }

    /// Build a client over an injected transport (tests/embedding).
    pub fn with_transport(
        config: PortalConfig,
        transport: Arc<dyn PortalTransport>,
    ) -> Result<Self, PortalError> {
        config.validate()?;
        Ok(Self { config, transport })
    }

    /// The configuration this client was built with.
    pub fn config(&self) -> &PortalConfig {
        &self.config
    }

    /// Probe the session bus, portal service name, and ScreenCast version.
    ///
    /// This is side-effect free and never shows a consent dialog. A missing
    /// service is [`PortalError::ServiceUnavailable`]; a missing session bus is
    /// [`PortalError::BusUnavailable`].
    pub async fn preflight(&self) -> Result<PortalPreflight, PortalError> {
        let timeout = self.config.preflight_timeout;
        let owner = bounded(
            "NameHasOwner",
            timeout,
            self.transport.name_owner(PORTAL_BUS_NAME),
        )
        .await?;
        let owner = owner.ok_or(PortalError::ServiceUnavailable)?;
        let screencast_version = bounded(
            "ScreenCast.version",
            timeout,
            self.transport.screencast_version(),
        )
        .await?;
        let required = self.config.source.minimum_version();
        if screencast_version < required {
            return Err(PortalError::VersionTooOld {
                found: screencast_version,
                required,
            });
        }
        Ok(PortalPreflight {
            bus_address: std::env::var("DBUS_SESSION_BUS_ADDRESS").ok(),
            owner,
            screencast_version,
        })
    }

    /// Run the full consent/selection handshake and return the selected node.
    ///
    /// This may block on the user's consent dialog for up to
    /// [`PortalConfig::response_timeout`]. A denial is
    /// [`PortalError::Denied`]; an absent portal or bus is a typed error. The
    /// session is closed best-effort when a later stage fails.
    pub async fn request_stream(&self) -> Result<PortalStream, PortalError> {
        self.preflight().await?;
        let timeout = self.config.response_timeout;

        let handle_token = request_token("create");
        let session_token = request_token("session");
        let created = bounded(
            "CreateSession",
            timeout,
            self.transport
                .create_session(&self.config, &handle_token, &session_token),
        )
        .await;
        let created = match created {
            Ok(results) => results,
            Err(error) => return Err(error),
        };
        let session = match created.session_handle() {
            Ok(session) => session,
            Err(error) => return Err(error),
        };

        let handle_token = request_token("select");
        if let Err(error) = bounded(
            "SelectSources",
            timeout,
            self.transport
                .select_sources(&session, &self.config, &handle_token),
        )
        .await
        {
            self.close_best_effort(&session).await;
            return Err(error);
        }

        let handle_token = request_token("start");
        let started = bounded(
            "Start",
            timeout,
            self.transport.start(&session, &self.config, &handle_token),
        )
        .await;
        let started = match started {
            Ok(results) => results,
            Err(error) => {
                self.close_best_effort(&session).await;
                return Err(error);
            }
        };
        let streams = match started.streams() {
            Ok(streams) => streams,
            Err(error) => {
                self.close_best_effort(&session).await;
                return Err(error);
            }
        };
        if streams.is_empty() {
            self.close_best_effort(&session).await;
            return Err(PortalError::MissingResult(
                "the portal returned an empty `streams` list".to_owned(),
            ));
        }

        // A missing fd is recorded rather than turned into a fake success. The
        // provider decides, via `require_pipewire_fd`, whether a node-id-only
        // handoff is acceptable for its compositor.
        let (pipewire_fd, pipewire_fd_error) = match bounded(
            "OpenPipeWireRemote",
            timeout,
            self.transport.open_pipewire_remote(&session),
        )
        .await
        {
            Ok(fd) => (Some(fd), None),
            Err(error) => (None, Some(error.to_string())),
        };

        Ok(PortalStream {
            session_handle: session,
            streams,
            pipewire_fd,
            pipewire_fd_error,
            restore_token: started.restore_token(),
            keepalive: Some(Arc::clone(&self.transport)),
        })
    }

    async fn close_best_effort(&self, session: &str) {
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            self.transport.close_session(session),
        )
        .await;
    }
}

async fn bounded<T>(
    stage: &str,
    timeout: Duration,
    future: impl Future<Output = Result<T, PortalError>>,
) -> Result<T, PortalError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| PortalError::Timeout {
            stage: stage.to_owned(),
            timeout,
        })?
}

/// Configuration for [`PortalCaptureProvider`].
#[derive(Clone, Debug)]
pub struct PortalCaptureConfig {
    /// Stable capture source id.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Require a portal `OpenPipeWireRemote` fd before advertising the source.
    pub require_pipewire_fd: bool,
    /// PipeWire options; the target is replaced by the portal node id on open.
    pub pipewire: PipeWireOptions,
}

impl Default for PortalCaptureConfig {
    fn default() -> Self {
        Self {
            id: PORTAL_DEFAULT_ID.to_owned(),
            name: PORTAL_DEFAULT_NAME.to_owned(),
            require_pipewire_fd: true,
            pipewire: PipeWireOptions {
                width: PORTAL_DEFAULT_WIDTH,
                height: PORTAL_DEFAULT_HEIGHT,
                fps: PORTAL_DEFAULT_FPS,
                ..PipeWireOptions::default()
            },
        }
    }
}

impl PortalCaptureConfig {
    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.id = id.into();
        self.name = name.into();
        self
    }

    /// Override the requested PipeWire mode.
    pub fn with_mode(mut self, width: u32, height: u32, fps: u32) -> Self {
        self.pipewire.width = width;
        self.pipewire.height = height;
        self.pipewire.fps = fps;
        self
    }

    /// Set whether a missing portal fd is a typed unsupported source.
    pub fn with_require_pipewire_fd(mut self, require: bool) -> Self {
        self.require_pipewire_fd = require;
        self
    }

    fn validate(&self) -> Result<(), ApiError> {
        if self.id.trim().is_empty() {
            return Err(ApiError::invalid(
                "The portal capture source requires a non-empty id.",
            ));
        }
        if self.name.trim().is_empty() {
            return Err(ApiError::invalid(
                "The portal capture source requires a non-empty name.",
            ));
        }
        if self.pipewire.width == 0 || self.pipewire.height == 0 {
            return Err(ApiError::invalid(
                "Portal capture dimensions must be non-zero.",
            ));
        }
        if self.pipewire.width > MAX_PORTAL_DIMENSION || self.pipewire.height > MAX_PORTAL_DIMENSION
        {
            return Err(ApiError::invalid(format!(
                "Portal capture dimensions {}x{} exceed the {MAX_PORTAL_DIMENSION}-pixel bound.",
                self.pipewire.width, self.pipewire.height
            )));
        }
        if self.pipewire.fps == 0 {
            return Err(ApiError::invalid(
                "The portal capture source requires a non-zero frame rate.",
            ));
        }
        Ok(())
    }
}

/// Portal session state kept alive for the lifetime of a capture source.
struct PortalSession {
    handle: String,
    fd: Mutex<Option<OwnedFd>>,
    /// Keeps the D-Bus connection (and therefore the portal session) open.
    _keepalive: Option<Arc<dyn PortalTransport>>,
}

impl fmt::Debug for PortalSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalSession")
            .field("handle", &self.handle)
            .field("has_fd", &self.has_fd())
            .field("keepalive", &self._keepalive.is_some())
            .finish()
    }
}

impl PortalSession {
    fn has_fd(&self) -> bool {
        self.fd
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some()
    }

    fn release(&self) {
        let _ = self
            .fd
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

/// A capture provider that opens the PipeWire node selected by a portal session.
///
/// The portal handshake itself is asynchronous and may prompt the user; call
/// [`PortalScreenCast::request_stream`] first, then register this provider with
/// [`CaptureRegistry::with_portal_capture`](crate::CaptureRegistry::with_portal_capture).
pub struct PortalCaptureProvider {
    config: PortalCaptureConfig,
    node_id: Option<u32>,
    stream: Option<PortalStreamInfo>,
    fd_error: Option<String>,
    session: Arc<PortalSession>,
}

impl fmt::Debug for PortalCaptureProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalCaptureProvider")
            .field("config", &self.config)
            .field("node_id", &self.node_id)
            .field("stream", &self.stream)
            .field("fd_error", &self.fd_error)
            .field("has_fd", &self.session.has_fd())
            .finish()
    }
}

impl PortalCaptureProvider {
    /// Build a provider from a successful portal stream.
    ///
    /// This consumes the stream's fd so it lives as long as any opened source.
    /// Configuration is validated at enumeration/open, not here, matching the
    /// other registry providers.
    pub fn new(config: PortalCaptureConfig, stream: PortalStream) -> Self {
        let node_id = stream.streams.first().map(|info| info.node_id);
        let info = stream.streams.first().cloned();
        let session = Arc::new(PortalSession {
            handle: stream.session_handle,
            fd: Mutex::new(stream.pipewire_fd),
            _keepalive: stream.keepalive,
        });
        Self {
            config,
            node_id,
            stream: info,
            fd_error: stream.pipewire_fd_error,
            session,
        }
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &PortalCaptureConfig {
        &self.config
    }

    /// Portal session object path.
    pub fn session_handle(&self) -> &str {
        &self.session.handle
    }

    /// True while the portal's `OpenPipeWireRemote` fd is held open.
    pub fn has_pipewire_fd(&self) -> bool {
        self.session.has_fd()
    }

    /// The selected stream metadata.
    pub fn stream(&self) -> Option<&PortalStreamInfo> {
        self.stream.as_ref()
    }

    /// PipeWire target for the selected node.
    pub fn target(&self) -> Result<StreamTarget, ApiError> {
        self.node_id.map(StreamTarget::NodeId).ok_or_else(|| {
            ApiError::new(
                codes::INVALID_REQUEST,
                "The portal stream has no selected PipeWire node.",
            )
            .with_remediation("Run the portal selection again and keep the selected monitor.")
        })
    }

    /// The capability advertised for this portal stream.
    pub fn capability(&self) -> Option<CaptureCapability> {
        if self.config.validate().is_err() {
            return None;
        }
        let node_id = self.node_id?;
        Some(CaptureCapability {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            // The portal returns a PipeWire Video/Source; the kind is not a
            // separate portal transport.
            kind: CaptureKind::PipeWire,
            formats: VideoPixelFormat::SUPPORTED
                .iter()
                .map(|format| format.label().to_owned())
                .collect(),
            max_width: self.config.pipewire.width,
            max_height: self.config.pipewire.height,
            max_fps: self.config.pipewire.fps,
            compressed: false,
            hdr: false,
            has_audio: false,
        })
        .filter(|_| node_id > 0)
    }

    fn fd_error(&self) -> ApiError {
        let detail = self
            .fd_error
            .clone()
            .unwrap_or_else(|| "the portal returned no OpenPipeWireRemote fd".to_owned());
        ApiError::new(
            codes::UNSUPPORTED,
            format!(
                "The portal selected PipeWire node {:?}, but no PipeWire remote fd is available.",
                self.node_id
            ),
        )
        .with_details(detail)
        .with_remediation(
            "Use a compositor/portal that implements OpenPipeWireRemote. Only set \
             require_pipewire_fd=false when the selected node is intentionally reachable \
             from the session PipeWire graph.",
        )
    }

    /// Reject enumeration when the session cannot be represented honestly.
    fn check_ready(&self) -> Result<(), ApiError> {
        self.config.validate()?;
        if self.node_id.unwrap_or(0) == 0 {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                "The portal stream has no non-zero PipeWire node id.",
            )
            .with_remediation("Run the portal selection again and keep the selected monitor."));
        }
        if self.config.require_pipewire_fd && !self.session.has_fd() {
            return Err(self.fd_error());
        }
        Ok(())
    }
}

impl CaptureProvider for PortalCaptureProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::PipeWire
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        self.check_ready()?;
        Ok(self.capability().into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::PipeWire || capability.id != self.config.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!(
                    "Portal capture source `{}` is not available.",
                    capability.id
                ),
            )
            .with_details(capability.id.clone()));
        }
        self.check_ready()?;
        let node_id = self.node_id.unwrap_or(0);
        let options = PipeWireOptions {
            target: StreamTarget::NodeId(node_id),
            width: self.config.pipewire.width,
            height: self.config.pipewire.height,
            fps: self.config.pipewire.fps,
            ..self.config.pipewire.clone()
        };
        // Reuse the existing PipeWire provider exactly: the portal contributes
        // selection/permission, not a second media path.
        let provider = PipeWireCaptureProvider::with_options(options);
        let inner_capability = CaptureCapability {
            id: format!("pipewire:node:{node_id}"),
            name: self.config.name.clone(),
            kind: CaptureKind::PipeWire,
            formats: capability.formats.clone(),
            max_width: capability.max_width,
            max_height: capability.max_height,
            max_fps: capability.max_fps,
            compressed: false,
            hdr: false,
            has_audio: false,
        };
        let inner = provider.open(&inner_capability)?;
        Ok(Box::new(PortalCaptureSource {
            id: self.config.id.clone(),
            inner,
            session: Arc::clone(&self.session),
        }))
    }
}

/// A capture source that keeps the portal fd/session alive while delegating
/// frames to the existing PipeWire source.
pub struct PortalCaptureSource {
    id: String,
    inner: Box<dyn CaptureSource>,
    session: Arc<PortalSession>,
}

impl fmt::Debug for PortalCaptureSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PortalCaptureSource")
            .field("id", &self.id)
            .field("session", &self.session.handle)
            .field("has_fd", &self.session.has_fd())
            .finish()
    }
}

#[async_trait]
impl CaptureSource for PortalCaptureSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> FrameFormat {
        self.inner.format()
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        self.inner.next_frame().await
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        let result = self.inner.stop().await;
        self.session.release();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn owned(value: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(value).expect("owned value")
    }

    fn results(entries: Vec<(&str, Value<'static>)>) -> PortalResults {
        PortalResults {
            entries: entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), owned(value)))
                .collect(),
        }
    }

    fn session_results(session: &str) -> PortalResults {
        results(vec![("session_handle", Value::from(session.to_owned()))])
    }

    fn stream_results(nodes: &[u32]) -> PortalResults {
        let streams: Vec<Value<'static>> = nodes
            .iter()
            .map(|node| {
                let props: HashMap<String, Value<'static>> = HashMap::new();
                Value::Structure(Structure::from((*node, Value::Dict(Dict::from(props)))))
            })
            .collect();
        results(vec![("streams", Value::Array(Array::from(streams)))])
    }

    fn empty_results() -> PortalResults {
        PortalResults::default()
    }

    #[derive(Default)]
    struct FakeTransport {
        owner: Mutex<Option<Result<Option<String>, PortalError>>>,
        version: Mutex<Option<Result<u32, PortalError>>>,
        create: Mutex<Option<Result<PortalResults, PortalError>>>,
        select: Mutex<Option<Result<PortalResults, PortalError>>>,
        start: Mutex<Option<Result<PortalResults, PortalError>>>,
        fd: Mutex<Option<Result<OwnedFd, PortalError>>>,
        closed: AtomicUsize,
        create_block: bool,
    }

    impl FakeTransport {
        fn ready() -> Self {
            Self {
                owner: Mutex::new(Some(Ok(Some(":1.42".to_owned())))),
                version: Mutex::new(Some(Ok(5))),
                create: Mutex::new(Some(Ok(session_results(
                    "/org/freedesktop/portal/desktop/session/1_42/ozma",
                )))),
                select: Mutex::new(Some(Ok(empty_results()))),
                start: Mutex::new(Some(Ok(stream_results(&[42])))),
                fd: Mutex::new(Some(Ok(fake_fd()))),
                ..Self::default()
            }
        }

        fn with_owner(self, owner: Option<String>) -> Self {
            *self.owner.lock().unwrap() = Some(Ok(owner));
            self
        }

        fn with_version(self, version: u32) -> Self {
            *self.version.lock().unwrap() = Some(Ok(version));
            self
        }

        fn with_bus_error(self, error: PortalError) -> Self {
            *self.owner.lock().unwrap() = Some(Err(error));
            self
        }

        fn with_select_error(self, error: PortalError) -> Self {
            *self.select.lock().unwrap() = Some(Err(error));
            self
        }

        fn with_start(self, start: PortalResults) -> Self {
            *self.start.lock().unwrap() = Some(Ok(start));
            self
        }

        fn with_fd_error(self, error: PortalError) -> Self {
            *self.fd.lock().unwrap() = Some(Err(error));
            self
        }
    }

    fn take<T>(slot: &Mutex<Option<Result<T, PortalError>>>) -> Result<T, PortalError> {
        slot.lock().unwrap().take().unwrap_or_else(|| {
            Err(PortalError::Bus {
                method: "fake".to_owned(),
                message: "scripted result was already consumed".to_owned(),
            })
        })
    }

    #[async_trait]
    impl PortalTransport for FakeTransport {
        async fn name_owner(&self, _name: &str) -> Result<Option<String>, PortalError> {
            take(&self.owner)
        }

        async fn screencast_version(&self) -> Result<u32, PortalError> {
            take(&self.version)
        }

        async fn create_session(
            &self,
            _config: &PortalConfig,
            _handle_token: &str,
            _session_handle_token: &str,
        ) -> Result<PortalResults, PortalError> {
            if self.create_block {
                std::future::pending::<()>().await;
            }
            take(&self.create)
        }

        async fn select_sources(
            &self,
            _session: &str,
            _config: &PortalConfig,
            _handle_token: &str,
        ) -> Result<PortalResults, PortalError> {
            take(&self.select)
        }

        async fn start(
            &self,
            _session: &str,
            _config: &PortalConfig,
            _handle_token: &str,
        ) -> Result<PortalResults, PortalError> {
            take(&self.start)
        }

        async fn open_pipewire_remote(&self, _session: &str) -> Result<OwnedFd, PortalError> {
            take(&self.fd)
        }

        async fn close_session(&self, _session: &str) {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn fake_fd() -> OwnedFd {
        std::fs::File::open("/dev/null")
            .expect("open /dev/null")
            .into()
    }

    fn client(transport: FakeTransport) -> PortalScreenCast {
        PortalScreenCast::with_transport(
            PortalConfig::default().with_response_timeout(Duration::from_millis(50)),
            Arc::new(transport),
        )
        .unwrap()
    }

    #[test]
    fn request_path_matches_the_portal_sender_token_scheme() {
        assert_eq!(
            portal_request_path(":1.42", "ozma_create_1"),
            "/org/freedesktop/portal/desktop/request/1_42/ozma_create_1"
        );
    }

    #[test]
    fn response_parsing_extracts_session_streams_and_props() {
        let mut props: HashMap<String, Value<'static>> = HashMap::new();
        props.insert(
            "position".to_owned(),
            Value::Structure(Structure::from((10i32, 20i32))),
        );
        props.insert(
            "size".to_owned(),
            Value::Structure(Structure::from((1920i32, 1080i32))),
        );
        props.insert("source_type".to_owned(), Value::from(1u32));
        props.insert("mapping_id".to_owned(), Value::from("mapping-7".to_owned()));
        let stream = Value::Structure(Structure::from((42u32, Value::Dict(Dict::from(props)))));
        let response = results(vec![
            ("session_handle", Value::from("/s".to_owned())),
            ("streams", Value::Array(Array::from(vec![stream]))),
            ("restore_token", Value::from("token-1".to_owned())),
        ]);

        assert_eq!(response.session_handle().unwrap(), "/s");
        let streams = response.streams().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].node_id, 42);
        assert_eq!(streams[0].position, Some((10, 20)));
        assert_eq!(streams[0].size, Some((1920, 1080)));
        assert_eq!(streams[0].source_type, Some(1));
        assert_eq!(streams[0].mapping_id.as_deref(), Some("mapping-7"));
        assert_eq!(response.restore_token().as_deref(), Some("token-1"));
    }

    #[test]
    fn malformed_responses_are_typed_not_empty_successes() {
        let missing = PortalResults::default();
        assert!(matches!(
            missing.session_handle(),
            Err(PortalError::MissingResult(_))
        ));
        assert!(matches!(
            missing.streams(),
            Err(PortalError::MissingResult(_))
        ));

        let wrong = results(vec![("streams", Value::from(7u32))]);
        assert!(matches!(
            wrong.streams(),
            Err(PortalError::MalformedResult { .. })
        ));

        let empty_node = results(vec![(
            "streams",
            Value::Array(Array::from(vec![Value::Structure(Structure::from((
                0u32,
                Value::Dict(Dict::from(HashMap::<String, Value<'static>>::new())),
            )))])),
        )]);
        assert_eq!(empty_node.streams().unwrap()[0].node_id, 0);
    }

    #[test]
    fn response_codes_map_to_success_denial_and_failure() {
        let mut successful_entries = HashMap::new();
        successful_entries.insert(
            "session_handle".to_owned(),
            owned(Value::from("/s".to_owned())),
        );
        let successful = response_to_results("Start", 0, successful_entries).unwrap();
        assert_eq!(successful.session_handle().unwrap(), "/s");

        let denied = response_to_results("Start", 1, HashMap::new()).unwrap_err();
        assert!(matches!(denied, PortalError::Denied { ref stage } if stage == "Start"));
        assert_eq!(denied.to_api_error().code, codes::CONSENT_DENIED);

        let mut failed_entries = HashMap::new();
        failed_entries.insert(
            "detail".to_owned(),
            owned(Value::from("no monitor".to_owned())),
        );
        let failed = response_to_results("Start", 2, failed_entries).unwrap_err();
        assert!(matches!(
            failed,
            PortalError::RequestFailed { response: 2, .. }
        ));
        assert!(failed.to_string().contains("no monitor"));
    }

    #[tokio::test]
    async fn successful_handshake_returns_node_and_fd() {
        let transport = FakeTransport::ready();
        let portal = client(transport);
        let stream = portal.request_stream().await.unwrap();

        assert_eq!(stream.node_id().unwrap(), 42);
        assert_eq!(stream.target().unwrap(), StreamTarget::NodeId(42));
        assert!(stream.pipewire_fd.is_some());
        assert!(stream.pipewire_fd_error.is_none());
        assert!(stream.keepalive.is_some());
    }

    #[tokio::test]
    async fn denial_is_typed_and_closes_the_session() {
        let closed = Arc::new(AtomicUsize::new(0));
        let inner = FakeTransport::ready().with_select_error(PortalError::Denied {
            stage: "SelectSources".to_owned(),
        });
        let transport = Arc::new(ObservedClose {
            inner,
            closed: Arc::clone(&closed),
        });
        let portal = PortalScreenCast::with_transport(
            PortalConfig::default().with_response_timeout(Duration::from_millis(50)),
            transport,
        )
        .unwrap();

        let error = portal.request_stream().await.unwrap_err();
        assert!(matches!(error, PortalError::Denied { .. }));
        assert_eq!(error.to_api_error().code, codes::CONSENT_DENIED);
        assert_eq!(closed.load(Ordering::Relaxed), 1);
    }

    struct ObservedClose {
        inner: FakeTransport,
        closed: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PortalTransport for ObservedClose {
        async fn name_owner(&self, name: &str) -> Result<Option<String>, PortalError> {
            self.inner.name_owner(name).await
        }

        async fn screencast_version(&self) -> Result<u32, PortalError> {
            self.inner.screencast_version().await
        }

        async fn create_session(
            &self,
            config: &PortalConfig,
            handle_token: &str,
            session_handle_token: &str,
        ) -> Result<PortalResults, PortalError> {
            self.inner
                .create_session(config, handle_token, session_handle_token)
                .await
        }

        async fn select_sources(
            &self,
            session: &str,
            config: &PortalConfig,
            handle_token: &str,
        ) -> Result<PortalResults, PortalError> {
            self.inner
                .select_sources(session, config, handle_token)
                .await
        }

        async fn start(
            &self,
            session: &str,
            config: &PortalConfig,
            handle_token: &str,
        ) -> Result<PortalResults, PortalError> {
            self.inner.start(session, config, handle_token).await
        }

        async fn open_pipewire_remote(&self, session: &str) -> Result<OwnedFd, PortalError> {
            self.inner.open_pipewire_remote(session).await
        }

        async fn close_session(&self, session: &str) {
            self.closed.fetch_add(1, Ordering::Relaxed);
            self.inner.close_session(session).await;
        }
    }

    #[tokio::test]
    async fn timeout_is_bounded_and_typed() {
        let mut transport = FakeTransport::ready();
        transport.create_block = true;
        let error = client(transport).request_stream().await.unwrap_err();
        assert!(matches!(
            error,
            PortalError::Timeout { ref stage, .. } if stage == "CreateSession"
        ));
        assert_eq!(error.to_api_error().code, codes::CAPTURE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn missing_service_and_bus_are_distinct_typed_errors() {
        let service = client(FakeTransport::ready().with_owner(None));
        assert!(matches!(
            service.request_stream().await.unwrap_err(),
            PortalError::ServiceUnavailable
        ));

        let bus = client(
            FakeTransport::ready().with_bus_error(PortalError::BusUnavailable {
                message: "no DBUS_SESSION_BUS_ADDRESS".to_owned(),
            }),
        );
        assert!(matches!(
            bus.request_stream().await.unwrap_err(),
            PortalError::BusUnavailable { .. }
        ));
    }

    #[tokio::test]
    async fn old_portal_version_is_a_typed_unsupported_request() {
        let portal = client(FakeTransport::ready().with_version(0));
        let error = portal.request_stream().await.unwrap_err();
        assert!(matches!(error, PortalError::VersionTooOld { .. }));
        assert_eq!(error.to_api_error().code, codes::UNSUPPORTED);
    }

    #[tokio::test]
    async fn missing_fd_is_recorded_and_never_a_silent_success() {
        let transport = FakeTransport::ready().with_fd_error(PortalError::PipeWireFdUnavailable {
            message: "portal replied UnknownMethod".to_owned(),
        });
        let stream = client(transport).request_stream().await.unwrap();
        assert_eq!(stream.node_id().unwrap(), 42);
        assert!(stream.pipewire_fd.is_none());
        assert!(stream
            .pipewire_fd_error
            .as_deref()
            .unwrap()
            .contains("UnknownMethod"));

        let provider = PortalCaptureProvider::new(PortalCaptureConfig::default(), stream);
        let error = provider.enumerate().unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);
        assert!(error.message.contains("no PipeWire remote fd"));
    }

    #[tokio::test]
    async fn node_only_handoff_requires_an_explicit_opt_out() {
        let transport = FakeTransport::ready().with_fd_error(PortalError::PipeWireFdUnavailable {
            message: "portal replied UnknownMethod".to_owned(),
        });
        let stream = client(transport).request_stream().await.unwrap();
        let config = PortalCaptureConfig::default().with_require_pipewire_fd(false);
        let provider = PortalCaptureProvider::new(config, stream);

        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].id, PORTAL_DEFAULT_ID);
        assert_eq!(capabilities[0].kind, CaptureKind::PipeWire);
        assert_eq!(provider.target().unwrap(), StreamTarget::NodeId(42));
    }

    #[tokio::test]
    async fn provider_rejects_missing_node_and_wrong_capability() {
        let transport = FakeTransport::ready().with_start(stream_results(&[]));
        let error = client(transport).request_stream().await.unwrap_err();
        assert!(matches!(error, PortalError::MissingResult(_)));

        let stream = PortalStream {
            session_handle: "/s".to_owned(),
            streams: vec![PortalStreamInfo {
                node_id: 42,
                position: None,
                size: None,
                source_type: None,
                mapping_id: None,
            }],
            pipewire_fd: Some(fake_fd()),
            pipewire_fd_error: None,
            restore_token: None,
            keepalive: None,
        };
        let provider = PortalCaptureProvider::new(PortalCaptureConfig::default(), stream);
        let mut capability = provider.capability().unwrap();

        capability.id = "other".into();
        assert_eq!(
            provider.open(&capability).err().unwrap().code,
            codes::NOT_FOUND
        );
        capability.id = PORTAL_DEFAULT_ID.into();
        capability.kind = CaptureKind::Vnc;
        assert_eq!(
            provider.open(&capability).err().unwrap().code,
            codes::NOT_FOUND
        );
    }

    #[test]
    fn config_validation_rejects_zero_bounds() {
        let config = PortalConfig::default().with_response_timeout(Duration::ZERO);
        assert!(matches!(
            PortalScreenCast::with_transport(config, Arc::new(FakeTransport::ready())).unwrap_err(),
            PortalError::InvalidConfig(_)
        ));

        let mut config = PortalCaptureConfig::default();
        config.pipewire.width = 0;
        let stream = PortalStream {
            session_handle: "/s".to_owned(),
            streams: vec![PortalStreamInfo {
                node_id: 42,
                position: None,
                size: None,
                source_type: None,
                mapping_id: None,
            }],
            pipewire_fd: Some(fake_fd()),
            pipewire_fd_error: None,
            restore_token: None,
            keepalive: None,
        };
        let provider = PortalCaptureProvider::new(config, stream);
        assert_eq!(
            provider.enumerate().unwrap_err().code,
            codes::INVALID_REQUEST
        );
    }

    #[tokio::test]
    #[ignore = "needs a live desktop portal; set OZMA_PORTAL_TEST=1"]
    async fn real_portal_preflight_and_optional_consent() {
        if std::env::var("OZMA_PORTAL_TEST").as_deref() != Ok("1") {
            eprintln!("skipping: set OZMA_PORTAL_TEST=1 on a desktop session");
            return;
        }
        let portal = PortalScreenCast::connect(
            PortalConfig::default().with_response_timeout(Duration::from_secs(120)),
        )
        .await
        .unwrap();
        let preflight = portal.preflight().await.unwrap();
        assert!(preflight.screencast_version >= MIN_SCREENCAST_VERSION);
        assert!(!preflight.owner.is_empty());

        if std::env::var("OZMA_PORTAL_CONSENT").as_deref() == Ok("1") {
            let stream = portal.request_stream().await.unwrap();
            assert!(stream.node_id().unwrap() > 0);
            // The caller may inspect `pipewire_fd`; this test intentionally
            // does not open a capture source or display any UI of its own.
            eprintln!(
                "portal selected node {} (fd: {})",
                stream.node_id().unwrap(),
                stream.pipewire_fd.is_some()
            );
        }
    }
}
