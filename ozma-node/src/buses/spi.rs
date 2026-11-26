// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, allowlisted SPI surface.
//!
//! The allowlist contains explicit absolute `/dev/spidevX.Y` paths. Route
//! segments are normalized to that form and compared lexically; there is no
//! glob, auto-detection, or fallback. Transfers are full-duplex, bounded by
//! `--spi-max-transfer-bytes` (or the shared transfer bound), mode 0–3, 8 or
//! 16 bits, and a bounded clock rate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};

use super::{
    encode_hex, invalid_request, io_failed, json_response, parse_hex_bytes, unavailable, BusError,
    BusLimits, BusState, MAX_ERROR_DETAIL, MAX_REQUEST_BODY_BYTES,
};

/// Default SPI clock rate when the request omits `speed_hz`.
pub const DEFAULT_SPEED_HZ: u32 = 1_000_000;
/// Lowest accepted SPI clock rate.
pub const MIN_SPEED_HZ: u32 = 1_000;
/// Highest accepted SPI clock rate.
pub const MAX_SPEED_HZ: u32 = 50_000_000;

/// Exact SPI device-node allowlist.
#[derive(Clone, Debug, Default)]
pub struct SpiConfig {
    devices: BTreeSet<PathBuf>,
}

impl SpiConfig {
    /// Parse comma/space separated `/dev/spidevX.Y` entries.
    pub fn from_entries<I, S>(entries: I) -> Result<Self, BusError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut config = Self::default();
        for entry in entries {
            for token in entry
                .as_ref()
                .split(|character: char| character == ',' || character.is_whitespace())
            {
                let token = token.trim();
                if token.is_empty() {
                    continue;
                }
                let path = validate_device_path(Path::new(token))?;
                config.devices.insert(path);
            }
        }
        config.validate()?;
        Ok(config)
    }

    /// Validate the absolute entry ceiling.
    pub fn validate(&self) -> Result<(), BusError> {
        if self.devices.len() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "SPI allowlist contains {} devices, above the {} absolute bound",
                self.devices.len(),
                super::MAX_DEVICES
            )));
        }
        for path in &self.devices {
            validate_device_path(path)?;
        }
        Ok(())
    }

    /// True when at least one device is allowlisted.
    pub fn is_configured(&self) -> bool {
        !self.devices.is_empty()
    }

    /// Number of allowlisted devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// True when no device is allowlisted.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Exact lexical allowlist check.
    pub fn allows(&self, device: &Path) -> bool {
        self.devices.contains(device)
    }

    /// Allowlisted paths in ascending order.
    pub fn devices(&self) -> impl Iterator<Item = &Path> {
        self.devices.iter().map(PathBuf::as_path)
    }
}

/// Validate and normalize an absolute `/dev/spidevX.Y` path.
pub fn validate_device_path(path: &Path) -> Result<PathBuf, BusError> {
    super::validate_absolute_path(path, "SPI device")?;
    if path.parent() != Some(Path::new("/dev")) {
        return Err(invalid_request(format!(
            "SPI device {} must be a direct child of /dev",
            path.display()
        )));
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(invalid_request("SPI device path has no UTF-8 basename"));
    };
    let Some(rest) = name.strip_prefix("spidev") else {
        return Err(invalid_request(
            "SPI device basename must be /dev/spidevX.Y",
        ));
    };
    let Some((bus, chip_select)) = rest.split_once('.') else {
        return Err(invalid_request(
            "SPI device basename must be /dev/spidevX.Y",
        ));
    };
    if bus.is_empty()
        || chip_select.is_empty()
        || !bus.bytes().all(|byte| byte.is_ascii_digit())
        || !chip_select.bytes().all(|byte| byte.is_ascii_digit())
        || chip_select.contains('.')
    {
        return Err(invalid_request(
            "SPI device basename must be /dev/spidevX.Y with numeric X and Y",
        ));
    }
    Ok(path.to_path_buf())
}

/// Normalize a route segment to an allowlist path.
///
/// Accepted forms are `spidev0.0`, `0.0`, `/dev/spidev0.0`, and
/// `dev/spidev0.0`. Anything else is rejected before the allowlist check.
pub fn resolve_device(raw: &str) -> Result<PathBuf, BusError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid_request("SPI device segment is empty"));
    }
    let name = raw
        .strip_prefix("/dev/")
        .or_else(|| raw.strip_prefix("dev/"))
        .unwrap_or(raw);
    if name.contains('/') || name.contains('\\') {
        return Err(invalid_request(format!(
            "invalid SPI device {raw:?}; expected spidevX.Y"
        )));
    }
    let name = if name.starts_with("spidev") {
        name.to_owned()
    } else if name.split_once('.').is_some_and(|(bus, cs)| {
        !bus.is_empty()
            && !cs.is_empty()
            && bus.bytes().all(|b| b.is_ascii_digit())
            && cs.bytes().all(|b| b.is_ascii_digit())
    }) {
        format!("spidev{name}")
    } else {
        return Err(invalid_request(format!(
            "invalid SPI device {raw:?}; expected spidevX.Y"
        )));
    };
    validate_device_path(&PathBuf::from("/dev").join(name))
}

/// `POST /buses/spi/{device}/transfer` body.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SpiTransferRequest {
    /// Full-duplex TX bytes as bounded hex.
    pub tx: String,
    /// SPI mode 0–3; defaults to 0.
    #[serde(default)]
    pub mode: Option<u8>,
    /// Clock rate in Hz; defaults to [`DEFAULT_SPEED_HZ`].
    #[serde(default)]
    pub speed_hz: Option<u32>,
    /// Bits per word: 8 or 16; defaults to 8.
    #[serde(default)]
    pub bits: Option<u8>,
}

/// Normalized transfer handed to a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiTransferOp {
    pub tx: Vec<u8>,
    pub mode: u8,
    pub speed_hz: u32,
    pub bits: u8,
}

/// SPI backend abstraction. Production uses [`RealSpiBackend`]; tests inject a
/// fake so the allowlist is observable before any device is opened.
pub trait SpiBackend: Send + Sync {
    /// Full-duplex transfer; returns exactly one byte per TX byte.
    fn transfer(&self, device: &Path, op: &SpiTransferOp) -> Result<Vec<u8>, BusError>;
}

/// Real Linux spidev backend using the safe `spidev` wrapper.
pub struct RealSpiBackend;

impl SpiBackend for RealSpiBackend {
    fn transfer(&self, device: &Path, op: &SpiTransferOp) -> Result<Vec<u8>, BusError> {
        #[cfg(target_os = "linux")]
        {
            transfer_real(device, op)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (device, op);
            Err(BusError::Unsupported {
                detail: "SPI is only implemented on Linux spidev".to_owned(),
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn transfer_real(device: &Path, op: &SpiTransferOp) -> Result<Vec<u8>, BusError> {
    use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};

    let mode = match op.mode {
        0 => SpiModeFlags::SPI_MODE_0,
        1 => SpiModeFlags::SPI_MODE_1,
        2 => SpiModeFlags::SPI_MODE_2,
        3 => SpiModeFlags::SPI_MODE_3,
        other => {
            return Err(invalid_request(format!(
                "SPI mode must be 0..=3, got {other}"
            )))
        }
    };
    let mut spi = Spidev::open(device)
        .map_err(|error| map_spi_error(&format!("opening {}", device.display()), error))?;
    let mut options = SpidevOptions::new();
    options
        .bits_per_word(op.bits)
        .max_speed_hz(op.speed_hz)
        .mode(mode);
    spi.configure(&options.build()).map_err(|error| {
        map_spi_error(
            &format!(
                "configuring {} mode {} {} Hz {} bits",
                device.display(),
                op.mode,
                op.speed_hz,
                op.bits
            ),
            error,
        )
    })?;
    let mut rx = vec![0u8; op.tx.len()];
    let mut transfer = SpidevTransfer::read_write(&op.tx, &mut rx);
    spi.transfer(&mut transfer).map_err(|error| {
        map_spi_error(
            &format!("transferring {} bytes on {}", op.tx.len(), device.display()),
            error,
        )
    })?;
    Ok(rx)
}

#[cfg(target_os = "linux")]
fn map_spi_error(context: &str, error: std::io::Error) -> BusError {
    let detail = format!("{context}: {error}");
    if error.kind() == std::io::ErrorKind::TimedOut || error.raw_os_error() == Some(libc::ETIMEDOUT)
    {
        BusError::Timeout {
            detail: detail.chars().take(MAX_ERROR_DETAIL).collect(),
        }
    } else if error.kind() == std::io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(libc::ENXIO) | Some(libc::ENODEV))
    {
        unavailable(detail)
    } else {
        io_failed(detail)
    }
}

/// One device in `GET /buses/spi`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpiDeviceInfo {
    pub device: String,
    pub available: bool,
}

/// `GET /buses/spi` response. This only stats allowlisted nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpiListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub devices: Vec<SpiDeviceInfo>,
    pub limits: BusLimits,
}

/// `POST /buses/spi/{device}/transfer` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpiTransferResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub device: String,
    pub mode: u8,
    pub speed_hz: u32,
    pub bits: u8,
    pub tx_bytes: usize,
    pub rx_bytes: usize,
    /// Exact RX bytes as lower-case hex.
    pub rx: String,
    pub encoding: &'static str,
}

impl BusState {
    pub(super) fn spi_list(&self) -> Result<SpiListResponse, BusError> {
        if !self.inner.config.spi.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let limit = self.inner.config.max_devices;
        let mut devices: Vec<SpiDeviceInfo> = self
            .inner
            .config
            .spi
            .devices()
            .map(|path| SpiDeviceInfo {
                device: path.display().to_string(),
                available: path.exists(),
            })
            .collect();
        let matched = devices.len();
        let truncated = matched > limit;
        devices.truncate(limit);
        Ok(SpiListResponse {
            ok: true,
            supported: true,
            configured: true,
            count: devices.len(),
            limit,
            truncated,
            devices,
            limits: self.limits(),
        })
    }

    pub(super) fn spi_transfer(
        &self,
        device_raw: &str,
        request: SpiTransferRequest,
    ) -> Result<SpiTransferResponse, BusError> {
        if !self.inner.config.spi.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let device = resolve_device(device_raw)?;
        if !self.inner.config.spi.allows(&device) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "SPI device {} is not allowlisted; add --spi-allow {}",
                    device.display(),
                    device.display()
                ),
            });
        }
        let tx = parse_hex_bytes(&request.tx, self.inner.config.max_transfer_bytes)?;
        if tx.is_empty() {
            return Err(invalid_request(
                "SPI transfer requires at least one TX byte",
            ));
        }
        let mode = request.mode.unwrap_or(0);
        if mode > 3 {
            return Err(invalid_request(format!(
                "SPI mode must be 0..=3, got {mode}"
            )));
        }
        let speed_hz = request.speed_hz.unwrap_or(DEFAULT_SPEED_HZ);
        if !(MIN_SPEED_HZ..=MAX_SPEED_HZ).contains(&speed_hz) {
            return Err(invalid_request(format!(
                "SPI speed_hz must be between {MIN_SPEED_HZ} and {MAX_SPEED_HZ}"
            )));
        }
        let bits = request.bits.unwrap_or(8);
        if !matches!(bits, 8 | 16) {
            return Err(invalid_request(format!(
                "SPI bits must be 8 or 16, got {bits}"
            )));
        }
        if bits == 16 && !tx.len().is_multiple_of(2) {
            return Err(invalid_request(
                "16-bit SPI transfers require an even number of TX bytes",
            ));
        }
        let op = SpiTransferOp {
            tx,
            mode,
            speed_hz,
            bits,
        };
        let mut rx = self.inner.spi.transfer(&device, &op)?;
        if rx.len() != op.tx.len() {
            return Err(io_failed(format!(
                "SPI backend returned {} RX bytes for {} TX bytes",
                rx.len(),
                op.tx.len()
            )));
        }
        rx.truncate(op.tx.len());
        Ok(SpiTransferResponse {
            ok: true,
            supported: true,
            configured: true,
            device: device.display().to_string(),
            mode,
            speed_hz,
            bits,
            tx_bytes: op.tx.len(),
            rx_bytes: rx.len(),
            rx: encode_hex(&rx),
            encoding: "hex",
        })
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/spi", get(list_handler))
        .route(
            "/buses/spi/:device/transfer",
            post(transfer_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        // Convenience alias for callers that spell the path as dev/spidevX.Y.
        .route(
            "/buses/spi/dev/:device/transfer",
            post(transfer_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.spi_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "SPI list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn transfer_handler(
    State(state): State<BusState>,
    AxumPath(device): AxumPath<String>,
    body: Result<axum::Json<SpiTransferRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.spi_transfer(&device, request)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "SPI transfer task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

fn rejection_response(rejection: JsonRejection) -> Response {
    let status = rejection.status();
    let detail = if status == StatusCode::PAYLOAD_TOO_LARGE {
        format!("request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte bound")
    } else {
        format!("invalid JSON body: {}", rejection.body_text())
    };
    error_response(invalid_request(detail))
}

fn error_response(error: BusError) -> Response {
    axum::response::IntoResponse::into_response(error)
}

fn bounded_detail(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::i2c::RealI2cBackend;
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tower::ServiceExt;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Transfer(PathBuf, SpiTransferOp),
    }

    #[derive(Default)]
    struct FakeInner {
        calls: Vec<Call>,
        results: VecDeque<Result<Vec<u8>, BusError>>,
    }

    #[derive(Clone, Default)]
    struct FakeSpi {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl FakeSpi {
        fn queue(&self, result: Result<Vec<u8>, BusError>) {
            self.inner.lock().unwrap().results.push_back(result);
        }

        fn calls(&self) -> Vec<Call> {
            self.inner.lock().unwrap().calls.clone()
        }
    }

    impl SpiBackend for FakeSpi {
        fn transfer(&self, device: &Path, op: &SpiTransferOp) -> Result<Vec<u8>, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner
                .calls
                .push(Call::Transfer(device.to_path_buf(), op.clone()));
            inner
                .results
                .pop_front()
                .unwrap_or_else(|| Ok(op.tx.clone()))
        }
    }

    fn config() -> BusConfig {
        BusConfig::new()
            .with_spi_allow(&["/dev/spidev0.0, /dev/spidev1.2".to_owned()])
            .expect("valid allowlist")
    }

    fn state(config: BusConfig, fake: FakeSpi) -> BusState {
        BusState::with_backends(config, Arc::new(RealI2cBackend), Arc::new(fake))
            .expect("valid state")
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
        (status, serde_json::from_slice(&bytes).unwrap())
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
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn allowlist_accepts_only_absolute_spidev_paths() {
        let config =
            SpiConfig::from_entries(["/dev/spidev0.0,/dev/spidev1.2 /dev/spidev0.0"]).unwrap();
        assert_eq!(config.len(), 2);
        assert!(config.allows(Path::new("/dev/spidev0.0")));
        assert!(config.allows(Path::new("/dev/spidev1.2")));
        assert!(!config.allows(Path::new("/dev/spidev2.0")));

        for bad in [
            "spidev0.0",
            "/dev/spidev0",
            "/dev/spidev0.",
            "/dev/spidev.0",
            "/dev/spidev0.x",
            "/dev/spidev0.0.0",
            "/dev/i2c-0",
            "/tmp/spidev0.0",
            "/dev/../dev/spidev0.0",
            "/dev/spidev0.0/extra",
        ] {
            assert!(
                SpiConfig::from_entries([bad]).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn route_device_segments_normalize_to_allowlist_paths() {
        for raw in ["spidev0.0", "0.0", "/dev/spidev0.0", "dev/spidev0.0"] {
            assert_eq!(
                resolve_device(raw).unwrap(),
                PathBuf::from("/dev/spidev0.0"),
                "{raw}"
            );
        }
        for bad in [
            "",
            "spidev0",
            "spidev0.0/extra",
            "../spidev0.0",
            "/dev/i2c-0",
            "spidev-1.0",
            "spidev0.-1",
        ] {
            assert!(resolve_device(bad).is_err(), "expected {bad:?} to fail");
        }
    }

    #[tokio::test]
    async fn list_only_stats_allowlisted_nodes() {
        let fake = FakeSpi::default();
        let app = router(state(config(), fake.clone()));
        let (status, body) = get(&app, "/buses/spi").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], true);
        assert_eq!(body["count"], 2);
        assert_eq!(body["devices"][0]["device"], "/dev/spidev0.0");
        assert_eq!(body["devices"][1]["device"], "/dev/spidev1.2");
        assert!(fake.calls().is_empty(), "listing must not open/transfer");
    }

    #[tokio::test]
    async fn transfer_is_bounded_and_reports_exact_rx() {
        let fake = FakeSpi::default();
        let app = router(state(config(), fake.clone()));

        let (status, body) = post(
            &app,
            "/buses/spi/spidev0.0/transfer",
            json!({"tx": "de ad be ef", "mode": 3, "speed_hz": 500000, "bits": 8}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["device"], "/dev/spidev0.0");
        assert_eq!(body["mode"], 3);
        assert_eq!(body["speed_hz"], 500_000);
        assert_eq!(body["bits"], 8);
        assert_eq!(body["tx_bytes"], 4);
        assert_eq!(body["rx_bytes"], 4);
        assert_eq!(body["rx"], "deadbeef");

        let (status, body) = post(
            &app,
            "/buses/spi/dev/spidev1.2/transfer",
            json!({"tx": "0102", "bits": 16}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["device"], "/dev/spidev1.2");
        assert_eq!(body["bits"], 16);
        assert_eq!(body["rx"], "0102");

        let calls = fake.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0],
            Call::Transfer(
                PathBuf::from("/dev/spidev0.0"),
                SpiTransferOp {
                    tx: vec![0xde, 0xad, 0xbe, 0xef],
                    mode: 3,
                    speed_hz: 500_000,
                    bits: 8,
                }
            )
        );
    }

    #[tokio::test]
    async fn unlisted_device_is_rejected_before_backend() {
        let fake = FakeSpi::default();
        let app = router(state(config(), fake.clone()));
        let (status, body) = post(&app, "/buses/spi/spidev9.9/transfer", json!({"tx": "00"})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
        assert!(fake.calls().is_empty());

        let (status, body) = post(
            &app,
            "/buses/spi/spidev0.0/transfer",
            json!({"tx": "00", "mode": 4}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn transfer_bounds_are_typed() {
        let fake = FakeSpi::default();
        let app = router(state(config(), fake.clone()));

        for (body, why) in [
            (json!({"tx": ""}), "empty tx"),
            (json!({"tx": "0"}), "odd hex"),
            (json!({"tx": "zz"}), "non-hex"),
            (json!({"tx": "00", "mode": 4}), "bad mode"),
            (json!({"tx": "00", "speed_hz": 0}), "low speed"),
            (json!({"tx": "00", "speed_hz": 50_000_001}), "high speed"),
            (json!({"tx": "00", "bits": 7}), "bad bits"),
            (json!({"tx": "000", "bits": 16}), "odd 16-bit tx"),
            (json!({"tx": "00", "unknown": true}), "unknown field"),
        ] {
            let (status, response) = post(&app, "/buses/spi/spidev0.0/transfer", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");
            assert_eq!(response["code"], "bus.invalid-request", "{why}");
        }

        let oversized = "00".repeat(4097);
        let (status, response) = post(
            &app,
            "/buses/spi/spidev0.0/transfer",
            json!({"tx": oversized}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(response["code"], "bus.invalid-request");

        assert!(
            fake.calls().is_empty(),
            "invalid requests must not reach the backend"
        );
    }

    #[tokio::test]
    async fn backend_failures_are_typed() {
        let fake = FakeSpi::default();
        fake.queue(Err(BusError::Unavailable {
            detail: "no /dev/spidev0.0".to_owned(),
        }));
        fake.queue(Err(BusError::Timeout {
            detail: "transfer timed out".to_owned(),
        }));
        let app = router(state(config(), fake));

        let (status, body) = post(&app, "/buses/spi/spidev0.0/transfer", json!({"tx": "00"})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");

        let (status, body) = post(&app, "/buses/spi/spidev0.0/transfer", json!({"tx": "00"})).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body["code"], "bus.timeout");
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed_and_does_not_open_anything() {
        let fake = FakeSpi::default();
        let app = router(state(BusConfig::default(), fake.clone()));
        let (status, body) = get(&app, "/buses/spi").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");

        let (status, body) = post(&app, "/buses/spi/spidev0.0/transfer", json!({"tx": "00"})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        assert!(fake.calls().is_empty());
    }
}
