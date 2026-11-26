// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Opt-in, bounded 1-Wire, I²C, and SPI bus surfaces.
//!
//! Nothing here is auto-detected or probed by default. Each protocol is only
//! usable after the operator supplies an explicit allowlist or flag:
//!
//! * `--onewire` enables the read-only Linux 1-Wire sysfs surface
//!   (`/sys/bus/w1/devices`), or an explicit sysfs root for containers/tests.
//! * `--i2c-allow BUS:ADDR,...` is the complete set of I²C bus/address pairs the
//!   node may touch. There is no implicit bus 0, no scan of unlisted buses, and
//!   reserved/broadcast addresses are rejected before any `open(2)`.
//! * `--spi-allow /dev/spidevX.Y` is the complete set of SPI device nodes the
//!   node may touch. Paths are validated and compared lexically; no globbing,
//!   auto-detection, or fallback device is used.
//!
//! Bounds are explicit: one SPI transfer, one I²C block, the number of listed
//! devices, and the `i2cdetect` scan deadline. Every route returns a typed
//! `bus.*` failure instead of an empty success. The router contains no
//! authentication: mount it **behind** the node bearer guard.
//!
//! # Mounting (pending wiring)
//!
//! `BusState` is intended to live on `controller_compat::ConnectionState` as a
//! `buses: BusState` field. The exact line to add in
//! `controller_compat::compat_router`, directly after the serial merge, is:
//!
//! ```ignore
//! .merge(crate::buses::router(state.buses.clone()))
//! ```
//!
//! If the field is stored as `Option<BusState>`, use
//! `.merge(crate::buses::router(state.buses.clone().unwrap_or_default()))`.
//! `BusState::default()` is a valid, fully unconfigured state: every implemented
//! route answers a typed `bus.not-configured` 503 and no device is opened.
//!
//! Build the state in `main` with [`BusConfig::from_cli`] (or
//! [`BusConfig::from_cli_with_limits`]) and pass it through. The CLI wiring is
//! deliberately left to the caller because `main.rs` is owned by another
//! in-flight change.

pub mod i2c;
pub mod onewire;
pub mod spi;

use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};

use onewire::{OneWire, OneWireConfig};

/// Default upper bound for one SPI transfer or one I²C block-oriented payload.
pub const DEFAULT_MAX_TRANSFER_BYTES: usize = 4096;
/// Hard upper bound for one SPI transfer (or a hex-encoded request body).
pub const MAX_TRANSFER_BYTES: usize = 64 * 1024;
/// SMBus block reads/writes carry at most 32 data bytes.
pub const MAX_BLOCK_BYTES: usize = 32;
/// Default SMBus block bound.
pub const DEFAULT_MAX_BLOCK_BYTES: usize = 32;
/// Default maximum number of devices/buses reported or allowlisted.
pub const DEFAULT_MAX_DEVICES: usize = 64;
/// Hard upper bound for devices/buses in one configuration or response.
pub const MAX_DEVICES: usize = 256;
/// Default 1-Wire sysfs root.
pub const DEFAULT_ONEWIRE_ROOT: &str = "/sys/bus/w1/devices";
/// Request-body ceiling: the largest hex payload plus JSON framing.
pub const MAX_REQUEST_BODY_BYTES: usize = MAX_TRANSFER_BYTES * 2 + 4096;
/// Longest error/detail string copied into a response or log.
pub const MAX_ERROR_DETAIL: usize = 400;

fn bounded(value: impl Into<String>) -> String {
    value.into().chars().take(MAX_ERROR_DETAIL).collect()
}

/// Typed bus failure with a stable `bus.*` code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BusError {
    /// No protocol was enabled with `--onewire`, `--i2c-allow`, or `--spi-allow`.
    #[error("no bus protocol is configured on this node")]
    NotConfigured,
    /// The bus/device is configured but absent, unreadable, or no longer present.
    #[error("bus or device unavailable: {detail}")]
    Unavailable { detail: String },
    /// The exact bus/address or device is not in the operator allowlist.
    #[error("not allowed by the node bus allowlist: {detail}")]
    NotAllowed { detail: String },
    /// A malformed id, body, operation, or out-of-range bound.
    #[error("invalid bus request: {detail}")]
    InvalidRequest { detail: String },
    /// The kernel/backend returned an I/O failure after the request was accepted.
    #[error("bus I/O failed: {detail}")]
    IoFailed { detail: String },
    /// This platform/backing tool cannot implement the requested operation.
    #[error("unsupported bus operation: {detail}")]
    Unsupported { detail: String },
    /// A bounded operation deadline expired.
    #[error("bus operation timed out: {detail}")]
    Timeout { detail: String },
}

impl BusError {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "bus.not-configured",
            Self::Unavailable { .. } => "bus.unavailable",
            Self::NotAllowed { .. } => "bus.not-allowed",
            Self::InvalidRequest { .. } => "bus.invalid-request",
            Self::IoFailed { .. } => "bus.io-failed",
            Self::Unsupported { .. } => "bus.unsupported",
            Self::Timeout { .. } => "bus.timeout",
        }
    }

    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotConfigured | Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotAllowed { .. } => StatusCode::FORBIDDEN,
            Self::InvalidRequest { .. } => StatusCode::BAD_REQUEST,
            Self::IoFailed { .. } => StatusCode::BAD_GATEWAY,
            Self::Unsupported { .. } => StatusCode::NOT_IMPLEMENTED,
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// Operator-facing remediation text.
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::NotConfigured => {
                "Start the node with --onewire, --i2c-allow BUS:ADDR,..., and/or --spi-allow /dev/spidevX.Y; nothing is probed without an explicit allowlist."
            }
            Self::Unavailable { .. } => {
                "Check the explicit sysfs root or device node; the node never falls back to a different bus or address."
            }
            Self::NotAllowed { .. } => {
                "Add the exact bus/address pair or /dev/spidevX.Y path to the corresponding --*-allow flag. The node never probes unlisted buses."
            }
            Self::InvalidRequest { .. } => {
                "Check the route id, JSON body, operation spelling, and the advertised limits."
            }
            Self::IoFailed { .. } => {
                "Inspect the node log and the underlying kernel driver; the requested bus/address was already allowlisted."
            }
            Self::Unsupported { .. } => {
                "Install i2c-tools (i2cdetect) or run on a platform with Linux i2c-dev/spidev support."
            }
            Self::Timeout { .. } => {
                "Check the bus wiring and reduce the scan or transfer size; the operation is strictly bounded."
            }
        }
    }

    fn body(&self) -> Value {
        json!({
            "ok": false,
            "supported": !matches!(self, Self::Unsupported { .. }),
            "configured": !matches!(self, Self::NotConfigured),
            "available": false,
            "code": self.code(),
            "error": self.to_string(),
            "remediation": self.remediation(),
        })
    }
}

impl IntoResponse for BusError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, Json(self.body())).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
    }
}

fn invalid_request(detail: impl Into<String>) -> BusError {
    BusError::InvalidRequest {
        detail: bounded(detail),
    }
}

fn io_failed(detail: impl Into<String>) -> BusError {
    BusError::IoFailed {
        detail: bounded(detail),
    }
}

fn unavailable(detail: impl Into<String>) -> BusError {
    BusError::Unavailable {
        detail: bounded(detail),
    }
}

/// Validated bus configuration.
///
/// `BusConfig::default()` is valid and enables nothing.
#[derive(Clone, Debug)]
pub struct BusConfig {
    /// Read-only 1-Wire sysfs root, or `None` when `--onewire` was not supplied.
    pub onewire: Option<OneWireConfig>,
    /// Exact I²C bus/address allowlist.
    pub i2c: i2c::I2cConfig,
    /// Exact SPI device-node allowlist.
    pub spi: spi::SpiConfig,
    /// Maximum bytes in one SPI transfer (and the hex payload bound).
    pub max_transfer_bytes: usize,
    /// Maximum SMBus block bytes.
    pub max_block_bytes: usize,
    /// Maximum devices/buses reported or allowlisted.
    pub max_devices: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            onewire: None,
            i2c: i2c::I2cConfig::default(),
            spi: spi::SpiConfig::default(),
            max_transfer_bytes: DEFAULT_MAX_TRANSFER_BYTES,
            max_block_bytes: DEFAULT_MAX_BLOCK_BYTES,
            max_devices: DEFAULT_MAX_DEVICES,
        }
    }
}

impl BusConfig {
    /// An unconfigured configuration with default bounds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable the read-only 1-Wire surface at `root`.
    pub fn with_onewire(mut self, root: impl Into<std::path::PathBuf>) -> Result<Self, BusError> {
        self.onewire = Some(OneWireConfig::new(root)?);
        Ok(self)
    }

    /// Replace the I²C allowlist. Each string may contain comma/space separated
    /// `BUS:ADDR` entries.
    pub fn with_i2c_allow(mut self, entries: &[String]) -> Result<Self, BusError> {
        self.i2c = i2c::I2cConfig::from_entries(entries)?;
        Ok(self)
    }

    /// Replace the SPI allowlist. Each string may contain comma/space separated
    /// `/dev/spidevX.Y` paths.
    pub fn with_spi_allow(mut self, entries: &[String]) -> Result<Self, BusError> {
        self.spi = spi::SpiConfig::from_entries(entries)?;
        Ok(self)
    }

    /// Set the transfer/block/device bounds.
    pub fn with_limits(
        mut self,
        max_transfer_bytes: usize,
        max_block_bytes: usize,
        max_devices: usize,
    ) -> Self {
        self.max_transfer_bytes = max_transfer_bytes;
        self.max_block_bytes = max_block_bytes;
        self.max_devices = max_devices;
        self
    }

    /// Build from the planned CLI values with default bounds.
    ///
    /// `onewire` follows clap bool semantics: `None` disables the surface,
    /// `Some("")`/`Some("true")` uses [`DEFAULT_ONEWIRE_ROOT`], and any other
    /// value is an explicit sysfs root.
    pub fn from_cli(
        onewire: Option<&str>,
        i2c_allow: &[String],
        spi_allow: &[String],
    ) -> Result<Self, BusError> {
        Self::from_cli_with_limits(
            onewire,
            i2c_allow,
            spi_allow,
            DEFAULT_MAX_TRANSFER_BYTES,
            DEFAULT_MAX_BLOCK_BYTES,
            DEFAULT_MAX_DEVICES,
        )
    }

    /// Build from the planned CLI values with explicit bounds.
    pub fn from_cli_with_limits(
        onewire: Option<&str>,
        i2c_allow: &[String],
        spi_allow: &[String],
        max_transfer_bytes: usize,
        max_block_bytes: usize,
        max_devices: usize,
    ) -> Result<Self, BusError> {
        let mut config =
            Self::default().with_limits(max_transfer_bytes, max_block_bytes, max_devices);
        if let Some(value) = onewire {
            let value = value.trim();
            let root = match value {
                "" | "true" | "1" => std::path::PathBuf::from(DEFAULT_ONEWIRE_ROOT),
                other => std::path::PathBuf::from(other),
            };
            config.onewire = Some(OneWireConfig::new(root)?);
        }
        config.i2c = i2c::I2cConfig::from_entries(i2c_allow)?;
        config.spi = spi::SpiConfig::from_entries(spi_allow)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate every bound and allowlist entry before any device is opened.
    pub fn validate(&self) -> Result<(), BusError> {
        if !(1..=MAX_TRANSFER_BYTES).contains(&self.max_transfer_bytes) {
            return Err(invalid_request(format!(
                "max transfer bytes must be between 1 and {MAX_TRANSFER_BYTES}"
            )));
        }
        if !(1..=MAX_BLOCK_BYTES).contains(&self.max_block_bytes) {
            return Err(invalid_request(format!(
                "max block bytes must be between 1 and {MAX_BLOCK_BYTES}"
            )));
        }
        if !(1..=MAX_DEVICES).contains(&self.max_devices) {
            return Err(invalid_request(format!(
                "max devices must be between 1 and {MAX_DEVICES}"
            )));
        }
        if let Some(onewire) = &self.onewire {
            onewire.validate()?;
        }
        self.i2c.validate()?;
        self.spi.validate()?;
        if self.i2c.total_addresses() > self.max_devices {
            return Err(invalid_request(format!(
                "I2C allowlist contains {} addresses, above the max-devices bound of {}",
                self.i2c.total_addresses(),
                self.max_devices
            )));
        }
        if self.spi.len() > self.max_devices {
            return Err(invalid_request(format!(
                "SPI allowlist contains {} devices, above the max-devices bound of {}",
                self.spi.len(),
                self.max_devices
            )));
        }
        Ok(())
    }
}

/// Advertised bus bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct BusLimits {
    pub max_transfer_bytes: usize,
    pub max_block_bytes: usize,
    pub max_devices: usize,
}

/// 1-Wire facts in `GET /buses/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OneWireStatus {
    pub configured: bool,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Number of valid device directories, bounded by `max_devices`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices: Option<usize>,
}

/// I²C facts in `GET /buses/status`. This is a configuration/metadata summary;
/// it never scans or probes a bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct I2cStatus {
    pub configured: bool,
    pub buses: usize,
    pub addresses: usize,
    pub available_buses: usize,
}

/// SPI facts in `GET /buses/status`. This only stats allowlisted nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SpiStatus {
    pub configured: bool,
    pub devices: usize,
    pub available_devices: usize,
}

/// `GET /buses/status` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BusStatus {
    pub ok: bool,
    pub supported: bool,
    /// These routes are mounted behind the node bearer guard.
    pub guarded: bool,
    pub onewire: OneWireStatus,
    pub i2c: I2cStatus,
    pub spi: SpiStatus,
    pub limits: BusLimits,
}

struct BusInner {
    config: BusConfig,
    onewire: Option<OneWire>,
    i2c: Arc<dyn i2c::I2cBackend>,
    spi: Arc<dyn spi::SpiBackend>,
}

/// Shared, opt-in bus service.
///
/// Construct with [`BusState::new`] for the real Linux backends, or
/// [`BusState::with_backends`] to inject fake/deterministic backends in tests.
#[derive(Clone)]
pub struct BusState {
    inner: Arc<BusInner>,
}

impl Default for BusState {
    fn default() -> Self {
        Self::new(BusConfig::default())
            .unwrap_or_else(|error| panic!("default bus configuration must be valid: {error}"))
    }
}

impl BusState {
    /// Build a service over the real Linux backends. No device is opened here.
    pub fn new(config: BusConfig) -> Result<Self, BusError> {
        Self::with_backends(
            config,
            Arc::new(i2c::RealI2cBackend),
            Arc::new(spi::RealSpiBackend),
        )
    }

    /// Build a service with explicit backends. No device is opened here.
    pub fn with_backends(
        config: BusConfig,
        i2c: Arc<dyn i2c::I2cBackend>,
        spi: Arc<dyn spi::SpiBackend>,
    ) -> Result<Self, BusError> {
        config.validate()?;
        let onewire = config.onewire.clone().map(OneWire::new);
        Ok(Self {
            inner: Arc::new(BusInner {
                config,
                onewire,
                i2c,
                spi,
            }),
        })
    }

    /// Effective validated configuration.
    pub fn config(&self) -> &BusConfig {
        &self.inner.config
    }

    /// Advertised bounds.
    pub fn limits(&self) -> BusLimits {
        BusLimits {
            max_transfer_bytes: self.inner.config.max_transfer_bytes,
            max_block_bytes: self.inner.config.max_block_bytes,
            max_devices: self.inner.config.max_devices,
        }
    }

    /// Configuration and metadata summary. This performs no I²C/SPI probing.
    pub fn status(&self) -> BusStatus {
        let onewire = match &self.inner.onewire {
            None => OneWireStatus {
                configured: false,
                available: false,
                root: None,
                devices: None,
            },
            Some(service) => {
                let available = service.root().is_dir();
                let devices = if available {
                    service
                        .listing(self.inner.config.max_devices)
                        .ok()
                        .map(|listing| listing.matched)
                } else {
                    None
                };
                OneWireStatus {
                    configured: true,
                    available,
                    root: Some(service.root().display().to_string()),
                    devices,
                }
            }
        };

        let mut buses = 0usize;
        let mut addresses = 0usize;
        let mut available_buses = 0usize;
        for bus in self.inner.config.i2c.bus_numbers() {
            buses += 1;
            addresses += self
                .inner
                .config
                .i2c
                .addresses(bus)
                .map_or(0, |set| set.len());
            if Path::new(&i2c::device_path(bus)).exists() {
                available_buses += 1;
            }
        }

        let devices = self.inner.config.spi.len();
        let available_devices = self
            .inner
            .config
            .spi
            .devices()
            .filter(|path| path.exists())
            .count();

        BusStatus {
            ok: true,
            supported: true,
            guarded: true,
            onewire,
            i2c: I2cStatus {
                configured: self.inner.config.i2c.is_configured(),
                buses,
                addresses,
                available_buses,
            },
            spi: SpiStatus {
                configured: self.inner.config.spi.is_configured(),
                devices,
                available_devices,
            },
            limits: self.limits(),
        }
    }
}

/// Router for the opt-in bus surface.
///
/// Mount it **behind** the node bearer guard; it deliberately contains no auth
/// logic of its own. With an unconfigured [`BusState`] every route returns a
/// typed `bus.not-configured` 503.
pub fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/status", get(status_handler))
        .with_state(state.clone())
        .merge(onewire::router(state.clone()))
        .merge(i2c::router(state.clone()))
        .merge(spi::router(state))
}

async fn status_handler(axum::extract::State(state): axum::extract::State<BusState>) -> Response {
    json_response(StatusCode::OK, state.status())
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: T) -> Response {
    let mut response = (status, Json(value)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

/// Validate an absolute device/sysfs path: absolute, no `.`/`..`, no empty
/// components. Existence is deliberately not required (the typed route reports
/// that later).
pub(crate) fn validate_absolute_path(path: &Path, what: &str) -> Result<(), BusError> {
    if path.as_os_str().is_empty() {
        return Err(invalid_request(format!("{what} path is empty")));
    }
    if !path.is_absolute() {
        return Err(invalid_request(format!("{what} path must be absolute")));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid_request(format!(
            "{what} path must not contain '.' or '..' components"
        )));
    }
    if !path
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(invalid_request(format!(
            "{what} path must name a filesystem entry"
        )));
    }
    Ok(())
}

/// Parse a bounded hex payload. Whitespace, `:`, `-`, and `_` are separators;
/// an initial `0x` is accepted. Returns at most `max` bytes.
pub(crate) fn parse_hex_bytes(input: &str, max: usize) -> Result<Vec<u8>, BusError> {
    let input = input.trim();
    let input = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"))
        .unwrap_or(input);
    let mut cleaned = String::with_capacity(input.len());
    for character in input.chars() {
        if character.is_ascii_whitespace() || matches!(character, ':' | '-' | '_') {
            continue;
        }
        if !character.is_ascii_hexdigit() {
            return Err(invalid_request(format!(
                "hex payload contains unexpected character {character:?}"
            )));
        }
        cleaned.push(character);
        if cleaned.len() > max.saturating_mul(2) {
            return Err(invalid_request(format!(
                "hex payload exceeds the {max}-byte bound"
            )));
        }
    }
    if !cleaned.len().is_multiple_of(2) {
        return Err(invalid_request(
            "hex payload has an odd number of digits".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks_exact(2) {
        let pair = std::str::from_utf8(pair).expect("ASCII hex digits");
        let byte = u8::from_str_radix(pair, 16)
            .map_err(|_| invalid_request("hex payload contains a non-hex pair".to_owned()))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Lower-case hex encoding used by read/transfer responses.
pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Parse a bounded duration from milliseconds, clamped by `min`/`max`.
pub(crate) fn bounded_timeout(
    millis: u64,
    min: u64,
    max: u64,
    what: &str,
) -> Result<Duration, BusError> {
    if !(min..=max).contains(&millis) {
        return Err(invalid_request(format!(
            "{what} timeout must be between {min} and {max} ms"
        )));
    }
    Ok(Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::path::Path;
    use tower::ServiceExt;

    fn entries(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[test]
    fn default_configuration_is_valid_and_enables_nothing() {
        let config = BusConfig::default();
        config.validate().unwrap();
        assert!(config.onewire.is_none());
        assert!(!config.i2c.is_configured());
        assert!(!config.spi.is_configured());
        assert_eq!(config.max_transfer_bytes, DEFAULT_MAX_TRANSFER_BYTES);
        assert_eq!(config.max_block_bytes, DEFAULT_MAX_BLOCK_BYTES);
        assert_eq!(config.max_devices, DEFAULT_MAX_DEVICES);
    }

    #[test]
    fn cli_parsing_enables_explicit_allowlists_only() {
        let i2c = entries(&["1:0x50,2:0x51"]);
        let spi = entries(&["/dev/spidev0.0"]);
        let config = BusConfig::from_cli(Some(""), &i2c, &spi).unwrap();
        assert_eq!(
            config.onewire.as_ref().unwrap().root(),
            Path::new(DEFAULT_ONEWIRE_ROOT)
        );
        assert!(config.i2c.allows(1, 0x50));
        assert!(config.i2c.allows(2, 0x51));
        assert!(config.spi.allows(Path::new("/dev/spidev0.0")));

        let config = BusConfig::from_cli(None, &[], &[]).unwrap();
        assert!(config.onewire.is_none());
        assert!(!config.i2c.is_configured());
        assert!(!config.spi.is_configured());
    }

    #[test]
    fn bounds_and_allowlist_size_are_enforced() {
        for config in [
            BusConfig::new().with_limits(0, 32, 64),
            BusConfig::new().with_limits(MAX_TRANSFER_BYTES + 1, 32, 64),
            BusConfig::new().with_limits(4096, 0, 64),
            BusConfig::new().with_limits(4096, MAX_BLOCK_BYTES + 1, 64),
            BusConfig::new().with_limits(4096, 32, 0),
            BusConfig::new().with_limits(4096, 32, MAX_DEVICES + 1),
        ] {
            assert!(matches!(
                config.validate(),
                Err(BusError::InvalidRequest { .. })
            ));
        }

        let too_many_i2c = entries(&["1:0x50,1:0x51,1:0x52"]);
        let config = BusConfig::new()
            .with_i2c_allow(&too_many_i2c)
            .unwrap()
            .with_limits(4096, 32, 2);
        assert!(matches!(
            config.validate(),
            Err(BusError::InvalidRequest { .. })
        ));

        let too_many_spi = entries(&["/dev/spidev0.0,/dev/spidev0.1,/dev/spidev0.2"]);
        let config = BusConfig::new()
            .with_spi_allow(&too_many_spi)
            .unwrap()
            .with_limits(4096, 32, 2);
        assert!(matches!(
            config.validate(),
            Err(BusError::InvalidRequest { .. })
        ));
    }

    #[tokio::test]
    async fn router_shapes_and_unconfigured_status_are_typed() {
        let app = router(BusState::default());

        let (status, body) = get(&app, "/buses/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["guarded"], true);
        assert_eq!(body["onewire"]["configured"], false);
        assert_eq!(body["i2c"]["configured"], false);
        assert_eq!(body["spi"]["configured"], false);
        assert_eq!(
            body["limits"]["max_transfer_bytes"],
            DEFAULT_MAX_TRANSFER_BYTES
        );

        for uri in [
            "/buses/1wire/devices",
            "/buses/1wire/28-000000000001",
            "/buses/i2c",
            "/buses/i2c/1/detect",
            "/buses/spi",
        ] {
            let (status, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["code"], "bus.not-configured", "{uri}");
        }

        for (uri, body) in [
            ("/buses/i2c/1/0x50/read", json!({"op": "byte"})),
            ("/buses/i2c/1/0x50/write", json!({"op": "byte", "value": 1})),
            ("/buses/spi/spidev0.0/transfer", json!({"tx": "00"})),
            ("/buses/spi/dev/spidev0.0/transfer", json!({"tx": "00"})),
        ] {
            let (status, body) = post(&app, uri, body).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["code"], "bus.not-configured", "{uri}");
        }

        let (status, _) = get(&app, "/buses/unknown").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/buses/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
