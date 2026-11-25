// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Opt-in, bounded Bluetooth management through the BlueZ `bluetoothctl` CLI.
//!
//! The service is deliberately conservative:
//!
//! * It is only created when the operator passes `--bluetooth`. A node without
//!   the flag keeps the routes mounted behind the bearer guard but every call
//!   returns a typed 503 `bluetooth.not-configured` document.
//! * Every mutating route requires an explicit MAC address in the request body.
//!   The address is validated before any process is started, and the exact
//!   `bluetoothctl` argv is `pair <addr>`, `connect <addr>`, `disconnect
//!   <addr>`, or `remove <addr>`. There is no shell, no command concatenation,
//!   and no implicit device selection.
//! * Pairing never trusts the device. This module never invokes
//!   `bluetoothctl trust`; trusting a device is an explicit operator action
//!   elsewhere.
//! * Scanning is bounded to [`MAX_SCAN_SECONDS`] and always attempts `scan off`
//!   afterwards, including after a timeout or a failed scan command.
//! * Lists are bounded: adapters, known devices, per-device `info` calls, and
//!   retained process output all have hard limits. A noisy scan cannot produce
//!   an unbounded response.
//! * A missing `bluetoothctl` binary or a host without an adapter is a typed
//!   503 `bluetooth.unavailable`; no route fabricates a device or a success.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Maximum number of adapters included in a status document.
pub const MAX_ADAPTERS: usize = 8;
/// Maximum number of known devices included in a devices/scan document.
pub const MAX_DEVICES: usize = 64;
/// Maximum number of per-device `info` calls used to enrich a list.
pub const MAX_DEVICE_DETAILS: usize = 32;
/// Default scan duration when the request does not specify one.
pub const DEFAULT_SCAN_SECONDS: u64 = 8;
/// Hard upper bound on a scan duration.
pub const MAX_SCAN_SECONDS: u64 = 15;
/// Hard lower bound on a scan duration.
pub const MIN_SCAN_SECONDS: u64 = 1;
/// Maximum stdout retained from one Bluetooth invocation.
pub const MAX_STDOUT_BYTES: usize = 256 * 1024;
/// Maximum stderr retained from one Bluetooth invocation.
pub const MAX_STDERR_BYTES: usize = 32 * 1024;
/// Maximum length of an error detail copied into a response.
const MAX_ERROR_DETAIL: usize = 400;
/// Overall budget for enriching a device list with per-device `info` calls.
const DEVICE_DETAIL_BUDGET: Duration = Duration::from_secs(8);

/// Timeouts for the individual operations.
#[derive(Debug, Clone)]
pub struct BluetoothTimeouts {
    pub list_adapters: Duration,
    pub adapter_status: Duration,
    pub list_devices: Duration,
    pub device_info: Duration,
    pub pair: Duration,
    pub connect: Duration,
    pub disconnect: Duration,
    pub forget: Duration,
    pub scan_stop: Duration,
}

impl Default for BluetoothTimeouts {
    fn default() -> Self {
        Self {
            list_adapters: Duration::from_secs(5),
            adapter_status: Duration::from_secs(5),
            list_devices: Duration::from_secs(5),
            device_info: Duration::from_secs(2),
            pair: Duration::from_secs(30),
            connect: Duration::from_secs(20),
            disconnect: Duration::from_secs(15),
            forget: Duration::from_secs(15),
            scan_stop: Duration::from_secs(3),
        }
    }
}

/// Validated Bluetooth configuration.
#[derive(Debug, Clone)]
pub struct BluetoothConfig {
    /// `bluetoothctl` executable (default `bluetoothctl`).
    pub program: PathBuf,
    /// Adapter MAC to use. `None` selects the `[default]` controller or the
    /// first controller BlueZ reports.
    pub adapter: Option<String>,
    pub timeouts: BluetoothTimeouts,
}

impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            program: PathBuf::from("bluetoothctl"),
            adapter: None,
            timeouts: BluetoothTimeouts::default(),
        }
    }
}

/// Typed Bluetooth failure. HTTP bodies never contain a shell command line or
/// any caller-supplied text beyond a validated MAC address.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BluetoothError {
    #[error("Bluetooth is not configured on this node")]
    NotConfigured,
    #[error("the bluetoothctl binary {program:?} is not available")]
    MissingBinary { program: String },
    #[error("no Bluetooth adapter is available")]
    NoAdapter,
    #[error("the configured Bluetooth adapter {address} was not found")]
    AdapterNotFound { address: String },
    #[error("the Bluetooth address is invalid; expected AA:BB:CC:DD:EE:FF")]
    InvalidAddress,
    #[error("the device limit must be between 1 and {MAX_DEVICES}")]
    InvalidLimit,
    #[error("another Bluetooth scan is already running")]
    Busy,
    #[error("bluetoothctl {operation} timed out")]
    Timeout { operation: &'static str },
    #[error("bluetoothctl {operation} failed: {detail}")]
    CommandFailed {
        operation: &'static str,
        detail: String,
    },
    #[error("pairing with {address} failed: {detail}")]
    PairFailed { address: String, detail: String },
    #[error("connecting to {address} failed: {detail}")]
    ConnectFailed { address: String, detail: String },
    #[error("disconnecting {address} failed: {detail}")]
    DisconnectFailed { address: String, detail: String },
    #[error("forgetting {address} failed: {detail}")]
    ForgetFailed { address: String, detail: String },
    #[error("Bluetooth I/O failed")]
    Io,
}

impl BluetoothError {
    /// Stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "bluetooth.not-configured",
            Self::MissingBinary { .. } | Self::NoAdapter | Self::AdapterNotFound { .. } => {
                "bluetooth.unavailable"
            }
            Self::InvalidAddress => "bluetooth.invalid-address",
            Self::InvalidLimit => "bluetooth.invalid-limit",
            Self::Busy => "bluetooth.busy",
            Self::Timeout { .. } => "bluetooth.timeout",
            Self::CommandFailed { .. } => "bluetooth.command-failed",
            Self::PairFailed { .. } => "bluetooth.pair-failed",
            Self::ConnectFailed { .. } => "bluetooth.connect-failed",
            Self::DisconnectFailed { .. } => "bluetooth.disconnect-failed",
            Self::ForgetFailed { .. } => "bluetooth.forget-failed",
            Self::Io => "bluetooth.io",
        }
    }

    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Busy => StatusCode::CONFLICT,
            Self::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            Self::CommandFailed { .. }
            | Self::PairFailed { .. }
            | Self::ConnectFailed { .. }
            | Self::DisconnectFailed { .. }
            | Self::ForgetFailed { .. } => StatusCode::BAD_GATEWAY,
            Self::NotConfigured
            | Self::MissingBinary { .. }
            | Self::NoAdapter
            | Self::AdapterNotFound { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Io => StatusCode::INTERNAL_SERVER_ERROR,
            Self::InvalidAddress | Self::InvalidLimit => StatusCode::BAD_REQUEST,
        }
    }

    /// Operator-facing remediation text.
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::NotConfigured => {
                "Start the node with --bluetooth to enable the guarded Bluetooth routes."
            }
            Self::MissingBinary { .. } => {
                "Install BlueZ (bluetoothctl) on this node or point --bluetooth-program at an executable."
            }
            Self::NoAdapter => {
                "Attach or enable a Bluetooth adapter (hci0); no adapter is fabricated."
            }
            Self::AdapterNotFound { .. } => {
                "Check --bluetooth-adapter or omit it to use the BlueZ default controller."
            }
            Self::InvalidAddress => {
                "Send exactly one six-octet MAC address, for example AA:BB:CC:DD:EE:FF."
            }
            Self::InvalidLimit => "Send a limit between 1 and 64.",
            Self::Busy => "Wait for the running scan to finish before starting another.",
            Self::Timeout { .. } => {
                "Retry with a shorter scan or check that BlueZ and the adapter are responsive."
            }
            Self::CommandFailed { .. }
            | Self::PairFailed { .. }
            | Self::ConnectFailed { .. }
            | Self::DisconnectFailed { .. }
            | Self::ForgetFailed { .. } => {
                "Check the bluez/bluetooth service and the node log; no action is reported as successful."
            }
            Self::Io => "Inspect the local process and BlueZ state.",
        }
    }

    fn body(&self) -> Value {
        json!({
            "ok": false,
            "supported": true,
            "configured": !matches!(self, Self::NotConfigured),
            "available": false,
            "code": self.code(),
            "error": self.to_string(),
            "remediation": self.remediation(),
        })
    }
}

impl IntoResponse for BluetoothError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, Json(self.body())).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
    }
}

/// One direct `bluetoothctl` invocation. The fake runner in tests records this
/// exact structure, so argv and bounds assertions exercise production code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothInvocation {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub max_stdout: usize,
}

/// Bounded result of one `bluetoothctl` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BluetoothOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
}

/// Runner abstraction. Production uses [`SystemBluetoothRunner`]; tests inject
/// a fake and inspect the exact argv or return typed failures.
#[async_trait]
pub trait BluetoothRunner: Send + Sync {
    /// Resolve the configured binary to an executable path.
    fn resolve(&self, configured: &Path) -> Result<PathBuf, BluetoothError>;

    /// Run one invocation. Implementations must bound both output streams and
    /// kill the child on timeout.
    async fn invoke(
        &self,
        invocation: BluetoothInvocation,
    ) -> Result<BluetoothOutput, BluetoothError>;
}

/// Production runner: direct exec, no shell, bounded output, kill on timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemBluetoothRunner;

#[async_trait]
impl BluetoothRunner for SystemBluetoothRunner {
    fn resolve(&self, configured: &Path) -> Result<PathBuf, BluetoothError> {
        resolve_program(configured)
    }

    async fn invoke(
        &self,
        invocation: BluetoothInvocation,
    ) -> Result<BluetoothOutput, BluetoothError> {
        run_system(invocation).await
    }
}

/// A configured, cloneable Bluetooth service.
#[derive(Clone)]
pub struct BluetoothService {
    inner: Arc<BluetoothInner>,
}

struct BluetoothInner {
    config: BluetoothConfig,
    runner: Arc<dyn BluetoothRunner>,
    scan_lock: tokio::sync::Mutex<()>,
    scan_active: AtomicBool,
}

/// One adapter as reported by `bluetoothctl list`/`show`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AdapterStatus {
    pub address: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub default: bool,
    pub powered: Option<bool>,
    pub discoverable: Option<bool>,
    pub discovering: Option<bool>,
    pub pairable: Option<bool>,
}

/// One known device, enriched by `bluetoothctl info` when available.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BluetoothDevice {
    pub address: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub connected: bool,
    pub blocked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address_type: Option<String>,
    /// Whether `bluetoothctl info` was successfully parsed for this row.
    pub detailed: bool,
}

/// Status document returned by `GET /bluetooth/status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BluetoothStatus {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub available: bool,
    pub program: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    pub adapters: Vec<AdapterStatus>,
    pub discovering: Option<bool>,
    pub scan: ScanLimits,
}

/// Bounded scan facts advertised by the status document and returned by a scan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScanLimits {
    pub active: bool,
    pub default_seconds: u64,
    pub max_seconds: u64,
}

/// Device list document returned by `GET /bluetooth/devices`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DeviceList {
    pub ok: bool,
    pub supported: bool,
    pub available: bool,
    pub count: usize,
    pub total: usize,
    pub limit: usize,
    pub truncated: bool,
    pub details_complete: bool,
    pub devices: Vec<BluetoothDevice>,
}

/// Scan result document returned by `POST /bluetooth/scan`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScanResult {
    pub ok: bool,
    pub supported: bool,
    pub available: bool,
    pub seconds: u64,
    pub cleanup: &'static str,
    pub total: usize,
    pub count: usize,
    pub truncated: bool,
    pub devices: Vec<BluetoothDevice>,
}

/// Static capability summary advertised on `/api/v1/connection`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BluetoothAdvertisement {
    pub configured: bool,
    pub adapter: Option<String>,
    pub max_scan_seconds: u64,
}

impl BluetoothService {
    /// Build a service with the production runner. This never probes the
    /// adapter; an unavailable host is reported by the first request.
    pub fn new(config: BluetoothConfig) -> Self {
        Self::with_runner(config, Arc::new(SystemBluetoothRunner))
    }

    /// Build a service with an injected runner (tests, or a future wrapper).
    pub fn with_runner(config: BluetoothConfig, runner: Arc<dyn BluetoothRunner>) -> Self {
        Self {
            inner: Arc::new(BluetoothInner {
                config,
                runner,
                scan_lock: tokio::sync::Mutex::new(()),
                scan_active: AtomicBool::new(false),
            }),
        }
    }

    pub fn config(&self) -> &BluetoothConfig {
        &self.inner.config
    }

    /// Static advertisement; no external process is started.
    pub fn advertisement(&self) -> BluetoothAdvertisement {
        BluetoothAdvertisement {
            configured: true,
            adapter: self.inner.config.adapter.clone(),
            max_scan_seconds: MAX_SCAN_SECONDS,
        }
    }

    /// Cheap startup probe: resolve `bluetoothctl` and list controllers.
    ///
    /// Used by `main` to decide whether the `bluetooth` registration capability
    /// is honest. It does not scan, pair, connect, or change adapter state.
    pub async fn probe(&self) -> Result<String, BluetoothError> {
        let adapters = self.list_adapters().await?;
        self.select_adapter(&adapters)
            .map(|adapter| adapter.address.clone())
    }

    /// Adapter(s), power/discoverable/discovering flags, and scan bounds.
    pub async fn status(&self) -> Result<BluetoothStatus, BluetoothError> {
        let adapters = self.list_adapters().await?;
        let selected = self.select_adapter(&adapters)?.address.clone();
        let mut detailed = Vec::with_capacity(adapters.len().min(MAX_ADAPTERS));
        let mut selected_status = None;
        for adapter in adapters.into_iter().take(MAX_ADAPTERS) {
            let mut status = adapter;
            match self
                .run(
                    &["show", status.address.as_str()],
                    self.inner.config.timeouts.adapter_status,
                    MAX_STDOUT_BYTES,
                )
                .await
            {
                Ok(output) => {
                    if let Some(parsed) = parse_adapter_show(&output.stdout) {
                        parsed.apply_to(&mut status);
                    }
                }
                Err(error) if status.address == selected => return Err(error),
                Err(_) => {}
            }
            if status.address == selected {
                selected_status = Some(status.clone());
            }
            detailed.push(status);
        }
        Ok(BluetoothStatus {
            ok: true,
            supported: true,
            configured: true,
            available: true,
            program: self.inner.config.program.display().to_string(),
            adapter: Some(selected),
            discovering: selected_status
                .as_ref()
                .and_then(|adapter| adapter.discovering),
            adapters: detailed,
            scan: ScanLimits {
                active: self.inner.scan_active.load(Ordering::SeqCst),
                default_seconds: DEFAULT_SCAN_SECONDS,
                max_seconds: MAX_SCAN_SECONDS,
            },
        })
    }

    /// Known/paired devices, bounded by `limit` (default [`MAX_DEVICES`]).
    pub async fn devices(&self, limit: Option<usize>) -> Result<DeviceList, BluetoothError> {
        let adapters = self.list_adapters().await?;
        let _selected = self.select_adapter(&adapters)?;
        let limit = normalize_limit(limit)?;
        let output = self
            .run(
                &["devices"],
                self.inner.config.timeouts.list_devices,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let (devices, total, details_complete) = self.enrich_devices(&output.stdout, limit).await;
        Ok(DeviceList {
            ok: true,
            supported: true,
            available: true,
            count: devices.len(),
            total,
            limit,
            truncated: total > limit,
            details_complete,
            devices,
        })
    }

    /// Run a bounded scan and return devices known to BlueZ afterwards.
    ///
    /// The `scan off` cleanup is attempted even when the scan command times out
    /// or fails. No pairing, trusting, or connection is attempted.
    pub async fn scan(&self, seconds: Option<u64>) -> Result<ScanResult, BluetoothError> {
        let guard = self
            .inner
            .scan_lock
            .try_lock()
            .map_err(|_| BluetoothError::Busy)?;
        self.inner.scan_active.store(true, Ordering::SeqCst);
        let seconds = seconds
            .unwrap_or(DEFAULT_SCAN_SECONDS)
            .clamp(MIN_SCAN_SECONDS, MAX_SCAN_SECONDS);
        let adapters = self.list_adapters().await;
        let result = match adapters {
            Err(error) => Err(error),
            Ok(adapters) => match self.select_adapter(&adapters) {
                Err(error) => Err(error),
                Ok(_) => self.scan_inner(seconds).await,
            },
        };
        // Always clean up discovery, including after a scan timeout. The
        // cleanup failure is intentionally ignored: BlueZ can report
        // `Failed to stop discovery` when the scan process already stopped it.
        let _ = self
            .run(
                &["scan", "off"],
                self.inner.config.timeouts.scan_stop,
                8 * 1024,
            )
            .await;
        self.inner.scan_active.store(false, Ordering::SeqCst);
        drop(guard);
        result
    }

    async fn scan_inner(&self, seconds: u64) -> Result<ScanResult, BluetoothError> {
        let attempt = self
            .run(
                &[
                    "--timeout".to_owned(),
                    seconds.to_string(),
                    "scan".to_owned(),
                    "on".to_owned(),
                ],
                Duration::from_secs(seconds.saturating_add(5)),
                MAX_STDOUT_BYTES,
            )
            .await;
        let scan_text = match attempt {
            Ok(output) => format!("{}\n{}", output.stdout, output.stderr),
            Err(error) => return Err(error),
        };
        let adapters = self.list_adapters().await?;
        self.select_adapter(&adapters)?;
        let output = self
            .run(
                &["devices"],
                self.inner.config.timeouts.list_devices,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let mut seen = parse_scan_log(&scan_text);
        let (mut devices, total, _) = self.enrich_devices(&output.stdout, MAX_DEVICES).await;
        for device in &mut devices {
            if let Some(rssi) = seen.remove(&device.address) {
                device.rssi = Some(rssi);
            }
        }
        Ok(ScanResult {
            ok: true,
            supported: true,
            available: true,
            seconds,
            cleanup: "scan-off",
            total,
            count: devices.len(),
            truncated: total > MAX_DEVICES,
            devices,
        })
    }

    /// Pair with one explicit address. Never calls `trust`.
    pub async fn pair(&self, address: &str) -> Result<Value, BluetoothError> {
        let address = normalize_address(address)?;
        let output = self
            .run(
                &["pair", address.as_str()],
                self.inner.config.timeouts.pair,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let text = combined_output(&output);
        if output.code != Some(0) || !is_success(&text, "pair") || is_failure(&text, "pair") {
            if is_no_adapter(&text) {
                return Err(BluetoothError::NoAdapter);
            }
            return Err(BluetoothError::PairFailed {
                address,
                detail: detail(&text),
            });
        }
        Ok(json!({
            "ok": true,
            "supported": true,
            "action": "pair",
            "address": address,
            "paired": true,
            "auto_trust": false,
        }))
    }

    /// Connect to one explicit address.
    pub async fn connect(&self, address: &str) -> Result<Value, BluetoothError> {
        let address = normalize_address(address)?;
        let output = self
            .run(
                &["connect", address.as_str()],
                self.inner.config.timeouts.connect,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let text = combined_output(&output);
        if output.code != Some(0) || !is_success(&text, "connect") || is_failure(&text, "connect") {
            if is_no_adapter(&text) {
                return Err(BluetoothError::NoAdapter);
            }
            return Err(BluetoothError::ConnectFailed {
                address,
                detail: detail(&text),
            });
        }
        Ok(json!({
            "ok": true,
            "supported": true,
            "action": "connect",
            "address": address,
            "connected": true,
        }))
    }

    /// Disconnect one explicit address.
    pub async fn disconnect(&self, address: &str) -> Result<Value, BluetoothError> {
        let address = normalize_address(address)?;
        let output = self
            .run(
                &["disconnect", address.as_str()],
                self.inner.config.timeouts.disconnect,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let text = combined_output(&output);
        if output.code != Some(0)
            || !is_success(&text, "disconnect")
            || is_failure(&text, "disconnect")
        {
            if is_no_adapter(&text) {
                return Err(BluetoothError::NoAdapter);
            }
            return Err(BluetoothError::DisconnectFailed {
                address,
                detail: detail(&text),
            });
        }
        Ok(json!({
            "ok": true,
            "supported": true,
            "action": "disconnect",
            "address": address,
            "connected": false,
        }))
    }

    /// Forget (remove) one explicit address.
    pub async fn forget(&self, address: &str) -> Result<Value, BluetoothError> {
        let address = normalize_address(address)?;
        let output = self
            .run(
                &["remove", address.as_str()],
                self.inner.config.timeouts.forget,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let text = combined_output(&output);
        if output.code != Some(0) || !is_success(&text, "forget") || is_failure(&text, "forget") {
            if is_no_adapter(&text) {
                return Err(BluetoothError::NoAdapter);
            }
            return Err(BluetoothError::ForgetFailed {
                address,
                detail: detail(&text),
            });
        }
        Ok(json!({
            "ok": true,
            "supported": true,
            "action": "forget",
            "address": address,
            "forgotten": true,
        }))
    }

    async fn list_adapters(&self) -> Result<Vec<AdapterStatus>, BluetoothError> {
        let output = self
            .run(
                &["list"],
                self.inner.config.timeouts.list_adapters,
                MAX_STDOUT_BYTES,
            )
            .await?;
        let adapters = parse_adapter_list(&output.stdout);
        if adapters.is_empty() {
            return Err(BluetoothError::NoAdapter);
        }
        Ok(adapters)
    }

    fn select_adapter<'a>(
        &self,
        adapters: &'a [AdapterStatus],
    ) -> Result<&'a AdapterStatus, BluetoothError> {
        if let Some(configured) = self.inner.config.adapter.as_deref() {
            let wanted = normalize_address(configured)?;
            return adapters
                .iter()
                .find(|adapter| adapter.address == wanted)
                .ok_or(BluetoothError::AdapterNotFound { address: wanted });
        }
        adapters
            .iter()
            .find(|adapter| adapter.default)
            .or_else(|| adapters.first())
            .ok_or(BluetoothError::NoAdapter)
    }

    async fn run(
        &self,
        args: &[impl AsRef<str>],
        timeout: Duration,
        max_stdout: usize,
    ) -> Result<BluetoothOutput, BluetoothError> {
        let program = self.inner.runner.resolve(&self.inner.config.program)?;
        let invocation = BluetoothInvocation {
            program,
            args: args.iter().map(|arg| arg.as_ref().to_owned()).collect(),
            timeout,
            max_stdout: max_stdout.min(MAX_STDOUT_BYTES),
        };
        self.inner.runner.invoke(invocation).await
    }

    async fn enrich_devices(
        &self,
        devices_output: &str,
        limit: usize,
    ) -> (Vec<BluetoothDevice>, usize, bool) {
        let all = parse_device_list(devices_output);
        let total = all.len();
        let mut devices: Vec<BluetoothDevice> = all.into_iter().take(limit).collect();
        let detail_budget = Instant::now() + DEVICE_DETAIL_BUDGET;
        let mut complete = true;
        for (index, device) in devices.iter_mut().enumerate() {
            if index >= MAX_DEVICE_DETAILS || Instant::now() >= detail_budget {
                complete = false;
                break;
            }
            match self
                .run(
                    &["info", device.address.as_str()],
                    self.inner.config.timeouts.device_info,
                    MAX_STDOUT_BYTES,
                )
                .await
            {
                Ok(output) => {
                    if let Some(parsed) = parse_device_info(&output.stdout) {
                        parsed.apply_to(device);
                        device.detailed = true;
                    } else {
                        complete = false;
                    }
                }
                Err(_) => complete = false,
            }
        }
        (devices, total, complete)
    }
}

#[derive(Debug, Deserialize, Default)]
struct ScanRequest {
    #[serde(default)]
    duration_seconds: Option<u64>,
    #[serde(default)]
    seconds: Option<u64>,
    #[serde(default)]
    timeout: Option<f64>,
}

impl ScanRequest {
    fn seconds(&self) -> Option<u64> {
        self.duration_seconds
            .or(self.seconds)
            .or_else(|| self.timeout.map(|value| value.max(0.0).floor() as u64))
    }
}

#[derive(Debug, Deserialize)]
struct AddressRequest {
    #[serde(default)]
    address: String,
}

#[derive(Debug, Deserialize)]
struct DeviceQuery {
    limit: Option<usize>,
}

/// Router for the opt-in Bluetooth surface. Mount this in the guarded
/// compatibility router; with no service every route answers a typed 503.
pub fn router(service: Option<BluetoothService>) -> Router {
    Router::new()
        .route("/bluetooth/status", get(status_handler))
        .route("/bluetooth/devices", get(devices_handler))
        .route("/bluetooth/scan", post(scan_handler))
        .route("/bluetooth/pair", post(pair_handler))
        .route("/bluetooth/connect", post(connect_handler))
        .route("/bluetooth/disconnect", post(disconnect_handler))
        .route("/bluetooth/forget", post(forget_handler))
        .with_state(BluetoothRouteState { service })
}

#[derive(Clone)]
struct BluetoothRouteState {
    service: Option<BluetoothService>,
}

fn json_no_store(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn not_configured() -> Response {
    BluetoothError::NotConfigured.into_response()
}

async fn status_handler(State(state): State<BluetoothRouteState>) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.status().await {
        Ok(status) => match serde_json::to_value(status) {
            Ok(body) => json_no_store(StatusCode::OK, body),
            Err(_) => BluetoothError::Io.into_response(),
        },
        Err(error) => error.into_response(),
    }
}

async fn devices_handler(
    State(state): State<BluetoothRouteState>,
    Query(query): Query<DeviceQuery>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.devices(query.limit).await {
        Ok(list) => match serde_json::to_value(list) {
            Ok(body) => json_no_store(StatusCode::OK, body),
            Err(_) => BluetoothError::Io.into_response(),
        },
        Err(error) => error.into_response(),
    }
}

async fn scan_handler(
    State(state): State<BluetoothRouteState>,
    body: Option<Json<ScanRequest>>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    let seconds = body.as_ref().and_then(|Json(request)| request.seconds());
    match service.scan(seconds).await {
        Ok(result) => match serde_json::to_value(result) {
            Ok(body) => json_no_store(StatusCode::OK, body),
            Err(_) => BluetoothError::Io.into_response(),
        },
        Err(error) => error.into_response(),
    }
}

async fn pair_handler(
    State(state): State<BluetoothRouteState>,
    Json(request): Json<AddressRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.pair(&request.address).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn connect_handler(
    State(state): State<BluetoothRouteState>,
    Json(request): Json<AddressRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.connect(&request.address).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn disconnect_handler(
    State(state): State<BluetoothRouteState>,
    Json(request): Json<AddressRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.disconnect(&request.address).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

async fn forget_handler(
    State(state): State<BluetoothRouteState>,
    Json(request): Json<AddressRequest>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.forget(&request.address).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

fn normalize_limit(limit: Option<usize>) -> Result<usize, BluetoothError> {
    match limit {
        None => Ok(MAX_DEVICES),
        Some(0) => Err(BluetoothError::InvalidLimit),
        Some(value) => Ok(value.min(MAX_DEVICES)),
    }
}

/// Validate and normalize a MAC address. Only the six-octet colon form is
/// accepted; the normalized value is uppercase and is the only caller text that
/// reaches argv.
fn normalize_address(address: &str) -> Result<String, BluetoothError> {
    let trimmed = address.trim();
    let parts: Vec<&str> = trimmed.split(':').collect();
    if parts.len() != 6 {
        return Err(BluetoothError::InvalidAddress);
    }
    let mut normalized = String::with_capacity(17);
    for (index, part) in parts.iter().enumerate() {
        if part.len() != 2 || !part.chars().all(|value| value.is_ascii_hexdigit()) {
            return Err(BluetoothError::InvalidAddress);
        }
        if index > 0 {
            normalized.push(':');
        }
        normalized.push_str(&part.to_ascii_uppercase());
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Default)]
struct ParsedAdapter {
    address: String,
    name: String,
    alias: Option<String>,
    default: bool,
    powered: Option<bool>,
    discoverable: Option<bool>,
    discovering: Option<bool>,
    pairable: Option<bool>,
}

impl ParsedAdapter {
    fn apply_to(&self, status: &mut AdapterStatus) {
        if !self.name.is_empty() {
            status.name = self.name.clone();
        }
        if self.alias.is_some() {
            status.alias = self.alias.clone();
        }
        status.default |= self.default;
        status.powered = self.powered.or(status.powered);
        status.discoverable = self.discoverable.or(status.discoverable);
        status.discovering = self.discovering.or(status.discovering);
        status.pairable = self.pairable.or(status.pairable);
    }
}

impl AdapterStatus {
    fn from_parsed(parsed: ParsedAdapter) -> Self {
        Self {
            address: parsed.address,
            name: parsed.name,
            alias: parsed.alias,
            default: parsed.default,
            powered: parsed.powered,
            discoverable: parsed.discoverable,
            discovering: parsed.discovering,
            pairable: parsed.pairable,
        }
    }
}

fn parse_adapter_list(output: &str) -> Vec<AdapterStatus> {
    let text = strip_ansi(output);
    let mut adapters = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("Controller ") else {
            continue;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let Some(address) = parts.next() else {
            continue;
        };
        if !valid_address(address) {
            continue;
        }
        let remainder = parts.next().unwrap_or("").trim();
        let default = remainder.to_ascii_lowercase().contains("[default]");
        let name = remainder.replace("[default]", "").trim().to_owned();
        let parsed = ParsedAdapter {
            address: address.to_ascii_uppercase(),
            name,
            default,
            ..ParsedAdapter::default()
        };
        adapters.push(AdapterStatus::from_parsed(parsed));
        if adapters.len() >= MAX_ADAPTERS {
            break;
        }
    }
    adapters
}

fn parse_adapter_show(output: &str) -> Option<ParsedAdapter> {
    let text = strip_ansi(output);
    let mut parsed = ParsedAdapter::default();
    let mut first = true;
    for line in text.lines() {
        let trimmed = line.trim();
        if first {
            if let Some(rest) = trimmed.strip_prefix("Controller ") {
                let mut parts = rest.split_whitespace();
                if let Some(address) = parts.next() {
                    if valid_address(address) {
                        parsed.address = address.to_ascii_uppercase();
                    }
                }
            }
            first = false;
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "Name" => parsed.name = value.to_owned(),
                "Alias" => parsed.alias = Some(value.to_owned()),
                "Powered" => parsed.powered = parse_bool(value),
                "Discoverable" => parsed.discoverable = parse_bool(value),
                "Discovering" => parsed.discovering = parse_bool(value),
                "Pairable" => parsed.pairable = parse_bool(value),
                _ => {}
            }
        }
    }
    if parsed.address.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

fn parse_device_list(output: &str) -> Vec<BluetoothDevice> {
    let text = strip_ansi(output);
    let mut devices: BTreeMap<String, BluetoothDevice> = BTreeMap::new();
    for line in text.lines() {
        let Some((address, name)) = device_line(line) else {
            continue;
        };
        devices
            .entry(address.clone())
            .or_insert_with(|| BluetoothDevice {
                address,
                name,
                alias: None,
                paired: false,
                bonded: false,
                trusted: false,
                connected: false,
                blocked: false,
                rssi: None,
                icon: None,
                class: None,
                address_type: None,
                detailed: false,
            });
    }
    devices.into_values().collect()
}

#[derive(Debug, Clone, Default)]
struct ParsedDeviceInfo {
    paired: Option<bool>,
    bonded: Option<bool>,
    trusted: Option<bool>,
    connected: Option<bool>,
    blocked: Option<bool>,
    name: Option<String>,
    alias: Option<String>,
    rssi: Option<i32>,
    icon: Option<String>,
    class: Option<String>,
    address_type: Option<String>,
}

impl ParsedDeviceInfo {
    fn apply_to(&self, device: &mut BluetoothDevice) {
        if let Some(value) = &self.name {
            device.name = value.clone();
        }
        if self.alias.is_some() {
            device.alias = self.alias.clone();
        }
        device.paired = self.paired.unwrap_or(device.paired);
        device.bonded = self.bonded.unwrap_or(device.bonded);
        device.trusted = self.trusted.unwrap_or(device.trusted);
        device.connected = self.connected.unwrap_or(device.connected);
        device.blocked = self.blocked.unwrap_or(device.blocked);
        device.rssi = self.rssi.or(device.rssi);
        if self.icon.is_some() {
            device.icon = self.icon.clone();
        }
        if self.class.is_some() {
            device.class = self.class.clone();
        }
        if self.address_type.is_some() {
            device.address_type = self.address_type.clone();
        }
    }
}

fn parse_device_info(output: &str) -> Option<ParsedDeviceInfo> {
    let text = strip_ansi(output);
    let mut parsed = ParsedDeviceInfo::default();
    let mut saw_device = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Device ") {
            saw_device = true;
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim();
            match key.trim() {
                "Name" => parsed.name = Some(value.to_owned()),
                "Alias" => parsed.alias = Some(value.to_owned()),
                "Paired" => parsed.paired = parse_bool(value),
                "Bonded" => parsed.bonded = parse_bool(value),
                "Trusted" => parsed.trusted = parse_bool(value),
                "Connected" => parsed.connected = parse_bool(value),
                "Blocked" => parsed.blocked = parse_bool(value),
                "RSSI" => parsed.rssi = parse_rssi(value),
                "Icon" => parsed.icon = Some(value.to_owned()),
                "Class" => parsed.class = Some(value.to_owned()),
                "AddressType" => parsed.address_type = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    if saw_device {
        Some(parsed)
    } else {
        None
    }
}

/// Parse `[NEW] Device AA:BB:... name` and `[CHG] Device AA:BB:... RSSI: ...`
/// lines from the asynchronous `bluetoothctl --timeout N scan on` output.
fn parse_scan_log(output: &str) -> BTreeMap<String, i32> {
    let text = strip_ansi(output);
    let mut seen = BTreeMap::new();
    for line in text.lines() {
        let Some((address, rest)) = device_line_with_rest(line) else {
            continue;
        };
        if let Some(value) = rest
            .find("RSSI:")
            .map(|index| rest[index + "RSSI:".len()..].trim())
            .and_then(parse_rssi)
        {
            seen.insert(address, value);
        } else {
            seen.entry(address).or_insert(0);
        }
    }
    // A zero placeholder from a NEW line must not overwrite a later real RSSI.
    seen.retain(|_, value| *value != 0);
    seen
}

fn device_line(line: &str) -> Option<(String, String)> {
    device_line_with_rest(line).map(|(address, rest)| {
        let name = rest.trim().to_owned();
        (address, name)
    })
}

fn device_line_with_rest(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("Device ").or_else(|| {
        trimmed
            .find("Device ")
            .map(|index| &trimmed[index + "Device ".len()..])
    })?;
    let mut parts = rest.splitn(2, char::is_whitespace);
    let address = parts.next()?;
    if !valid_address(address) {
        return None;
    }
    Some((
        address.to_ascii_uppercase(),
        parts.next().unwrap_or("").trim_start().to_owned(),
    ))
}

fn valid_address(value: &str) -> bool {
    let parts: Vec<&str> = value.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.chars().all(|char| char.is_ascii_hexdigit()))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" => Some(true),
        "no" | "false" | "off" => Some(false),
        _ => None,
    }
}

fn parse_rssi(value: &str) -> Option<i32> {
    let value = value.trim();
    if let Some(start) = value.find('(') {
        if let Some(end) = value[start + 1..].find(')') {
            if let Ok(parsed) = value[start + 1..start + 1 + end].trim().parse() {
                return Some(parsed);
            }
        }
    }
    value
        .split_whitespace()
        .find_map(|token| token.parse::<i32>().ok())
}

fn strip_ansi(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(char) = chars.next() {
        if char == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            output.push(char);
        }
    }
    output
}

fn combined_output(output: &BluetoothOutput) -> String {
    format!("{}\n{}", output.stdout, output.stderr)
}

fn detail(text: &str) -> String {
    let cleaned = strip_ansi(text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with("Attempting"))
        .filter(|line| !line.starts_with("Changing"))
        .collect::<Vec<_>>()
        .join("; ");
    if cleaned.is_empty() {
        "the command produced no detail".to_owned()
    } else if cleaned.len() > MAX_ERROR_DETAIL {
        let truncated: String = cleaned.chars().take(MAX_ERROR_DETAIL).collect();
        format!("{truncated}…")
    } else {
        cleaned
    }
}

/// Require a positive BlueZ confirmation before reporting a mutation as
/// successful. A zero exit with empty or unrecognised output is a typed failure,
/// never a success-shaped response.
fn is_success(text: &str, action: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    match action {
        "pair" => lower.contains("pairing successful") || lower.contains("paired: yes"),
        "connect" => lower.contains("connection successful") || lower.contains("connected: yes"),
        "disconnect" => {
            lower.contains("successful disconnected")
                || lower.contains("disconnected")
                || lower.contains("connected: no")
        }
        "forget" => {
            lower.contains("device has been removed")
                || lower.contains("removed")
                || lower.contains("forget successful")
                || lower.contains("removing device")
                || lower.contains("[del]")
        }
        _ => false,
    }
}

/// BlueZ reports a missing/unpowered controller as a command failure, not as an
/// adapter-list result. Map it to the same typed 503 as `/bluetooth/status`.
fn is_no_adapter(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no default controller")
        || lower.contains("no controller available")
        || lower.contains("no bluetooth controller")
        || lower.contains("controller not available")
}

fn is_failure(text: &str, action: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    match action {
        "pair" => {
            lower.contains("failed to pair")
                || lower.contains("pairing failed")
                || lower.contains("authenticationfailed")
        }
        "connect" => {
            lower.contains("failed to connect")
                || lower.contains("connection failed")
                || lower.contains("not available")
                || lower.contains("br-connection-")
        }
        "disconnect" => lower.contains("failed to disconnect") || lower.contains("not connected"),
        "forget" => {
            lower.contains("failed to remove")
                || lower.contains("does not exist")
                || lower.contains("not available")
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// System process runner
// ---------------------------------------------------------------------------

fn resolve_program(configured: &Path) -> Result<PathBuf, BluetoothError> {
    if configured.is_absolute() || configured.components().count() > 1 {
        return executable(configured).ok_or_else(|| BluetoothError::MissingBinary {
            program: configured.display().to_string(),
        });
    }
    let path = std::env::var_os("PATH")
        .unwrap_or_else(|| std::ffi::OsString::from("/usr/local/bin:/usr/bin:/bin"));
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(configured);
        if let Some(candidate) = executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(BluetoothError::MissingBinary {
        program: configured.display().to_string(),
    })
}

fn executable(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(path.to_path_buf())
}

async fn run_system(invocation: BluetoothInvocation) -> Result<BluetoothOutput, BluetoothError> {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    command
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return match error.kind() {
                std::io::ErrorKind::NotFound => Err(BluetoothError::MissingBinary {
                    program: invocation.program.display().to_string(),
                }),
                std::io::ErrorKind::PermissionDenied => Err(BluetoothError::Io),
                _ => Err(BluetoothError::Io),
            };
        }
    };
    let stdout = child.stdout.take().ok_or(BluetoothError::Io)?;
    let stderr = child.stderr.take().ok_or(BluetoothError::Io)?;

    let stdout_limit = invocation.max_stdout.min(MAX_STDOUT_BYTES);
    let wait = async { child.wait().await };
    let read = async {
        let (stdout, stdout_truncated) = read_limited(stdout, stdout_limit).await?;
        let (stderr, _) = read_limited(stderr, MAX_STDERR_BYTES).await?;
        let status = wait.await?;
        Ok::<_, std::io::Error>((stdout, stdout_truncated, stderr, status))
    };

    match tokio::time::timeout(invocation.timeout, read).await {
        Ok(Ok((stdout, stdout_truncated, stderr, status))) => Ok(BluetoothOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_truncated,
        }),
        Ok(Err(_)) => Err(BluetoothError::Io),
        Err(_) => Err(BluetoothError::Timeout {
            operation: "bluetoothctl",
        }),
    }
}

async fn read_limited<R>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8192];
    let mut output = Vec::new();
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if output.len() < limit {
            let take = read.min(limit - output.len());
            output.extend_from_slice(&buffer[..take]);
            if take < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok((output, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const ADDRESS: &str = "AA:BB:CC:DD:EE:FF";
    const ADAPTER: &str = "80:84:89:F6:E2:E8";

    #[derive(Debug, Clone)]
    struct FakeResponse {
        output: Result<BluetoothOutput, BluetoothError>,
    }

    impl FakeResponse {
        fn ok(stdout: &str) -> Self {
            Self {
                output: Ok(BluetoothOutput {
                    code: Some(0),
                    stdout: stdout.to_owned(),
                    stderr: String::new(),
                    stdout_truncated: false,
                }),
            }
        }

        fn stderr(stderr: &str) -> Self {
            Self {
                output: Ok(BluetoothOutput {
                    code: Some(0),
                    stdout: String::new(),
                    stderr: stderr.to_owned(),
                    stdout_truncated: false,
                }),
            }
        }

        fn timeout() -> Self {
            Self {
                output: Err(BluetoothError::Timeout {
                    operation: "bluetoothctl",
                }),
            }
        }
    }

    struct FakeRunner {
        calls: Mutex<Vec<BluetoothInvocation>>,
        responses: Mutex<VecDeque<FakeResponse>>,
        available: bool,
    }

    impl FakeRunner {
        fn new(available: bool) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
                available,
            }
        }

        fn push(&self, response: FakeResponse) {
            self.responses.lock().unwrap().push_back(response);
        }

        fn calls(&self) -> Vec<BluetoothInvocation> {
            self.calls.lock().unwrap().clone()
        }

        fn args(&self) -> Vec<Vec<String>> {
            self.calls()
                .into_iter()
                .map(|invocation| invocation.args)
                .collect()
        }
    }

    #[async_trait]
    impl BluetoothRunner for FakeRunner {
        fn resolve(&self, _configured: &Path) -> Result<PathBuf, BluetoothError> {
            if self.available {
                Ok(PathBuf::from("/usr/bin/bluetoothctl"))
            } else {
                Err(BluetoothError::MissingBinary {
                    program: "bluetoothctl".to_owned(),
                })
            }
        }

        async fn invoke(
            &self,
            invocation: BluetoothInvocation,
        ) -> Result<BluetoothOutput, BluetoothError> {
            self.calls.lock().unwrap().push(invocation);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| FakeResponse::ok(""))
                .output
        }
    }

    fn service_for(runner: Arc<FakeRunner>) -> BluetoothService {
        BluetoothService::with_runner(BluetoothConfig::default(), runner)
    }

    fn list_output() -> &'static str {
        "Controller 80:84:89:F6:E2:E8 SFFPC [default]\n"
    }

    fn show_output() -> &'static str {
        "Controller 80:84:89:F6:E2:E8 (public)\n\tName: SFFPC\n\tAlias: SFFPC\n\tPowered: yes\n\tDiscoverable: no\n\tDiscovering: no\n\tPairable: yes\n"
    }

    fn device_line(index: usize) -> String {
        format!("Device AA:BB:CC:DD:EE:{index:02X} Device {index}\n")
    }

    #[test]
    fn addresses_are_validated_before_any_command() {
        for value in [
            "",
            "AA:BB:CC:DD:EE",
            "AA:BB:CC:DD:EE:FF:00",
            "AA:BB:CC:DD:EE:GG",
            "not-a-mac",
            "AA-BB-CC-DD-EE-FF",
            "AA:BB:CC:DD:EE:FF; reboot",
        ] {
            assert_eq!(
                normalize_address(value),
                Err(BluetoothError::InvalidAddress),
                "{value:?} must be rejected"
            );
        }
        assert_eq!(normalize_address(" aa:bb:cc:dd:ee:ff ").unwrap(), ADDRESS);
    }

    #[tokio::test]
    async fn invalid_address_starts_no_process() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        let error = service.pair("not-a-mac").await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.invalid-address");
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn actions_use_exact_argv_and_never_trust() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok("Pairing successful\n"));
        runner.push(FakeResponse::ok("Connection successful\n"));
        runner.push(FakeResponse::ok("Successful disconnected\n"));
        runner.push(FakeResponse::ok("Device has been removed\n"));

        let paired = service.pair(ADDRESS).await.unwrap();
        assert_eq!(paired["paired"], true);
        assert_eq!(paired["auto_trust"], false);
        let connected = service.connect(ADDRESS).await.unwrap();
        assert_eq!(connected["connected"], true);
        let disconnected = service.disconnect(ADDRESS).await.unwrap();
        assert_eq!(disconnected["connected"], false);
        let forgotten = service.forget(ADDRESS).await.unwrap();
        assert_eq!(forgotten["forgotten"], true);

        let expected = vec![
            vec!["pair".to_owned(), ADDRESS.to_owned()],
            vec!["connect".to_owned(), ADDRESS.to_owned()],
            vec!["disconnect".to_owned(), ADDRESS.to_owned()],
            vec!["remove".to_owned(), ADDRESS.to_owned()],
        ];
        assert_eq!(runner.args(), expected);
        for call in runner.args() {
            assert!(
                !call.iter().any(|arg| arg == "trust"),
                "pairing/connect must never auto-trust"
            );
        }
    }

    #[tokio::test]
    async fn zero_exit_without_confirmation_is_not_a_fake_success() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(""));
        let error = service.pair(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.pair-failed");

        runner.push(FakeResponse::ok("Attempting to connect\n"));
        let error = service.connect(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.connect-failed");

        runner.push(FakeResponse::ok(""));
        let error = service.disconnect(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.disconnect-failed");

        runner.push(FakeResponse::ok(""));
        let error = service.forget(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.forget-failed");
    }

    #[tokio::test]
    async fn action_without_an_adapter_is_a_typed_503() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::stderr("No default controller available\n"));
        let error = service.connect(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.unavailable");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn pair_and_connect_failures_are_typed() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::stderr(
            "Failed to pair: org.bluez.Error.AuthenticationFailed\n",
        ));
        let error = service.pair(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.pair-failed");
        assert_eq!(error.status(), StatusCode::BAD_GATEWAY);

        runner.push(FakeResponse::stderr(
            "Failed to connect: org.bluez.Error.Failed\n",
        ));
        let error = service.connect(ADDRESS).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.connect-failed");
    }

    #[tokio::test]
    async fn status_parses_adapters_and_flags() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(list_output()));
        runner.push(FakeResponse::ok(show_output()));

        let status = service.status().await.unwrap();
        assert_eq!(status.adapter.as_deref(), Some(ADAPTER));
        assert_eq!(status.adapters.len(), 1);
        assert_eq!(status.adapters[0].powered, Some(true));
        assert_eq!(status.adapters[0].discoverable, Some(false));
        assert_eq!(status.discovering, Some(false));
        assert_eq!(status.scan.max_seconds, MAX_SCAN_SECONDS);
        assert_eq!(
            runner.args(),
            vec![
                vec!["list".to_owned()],
                vec!["show".to_owned(), ADAPTER.to_owned()],
            ]
        );
    }

    #[tokio::test]
    async fn devices_parse_flags_rssi_and_are_bounded() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(list_output()));
        let mut list = String::new();
        for index in 0..(MAX_DEVICES + 10) {
            list.push_str(&device_line(index));
        }
        runner.push(FakeResponse::ok(&list));
        for _ in 0..MAX_DEVICE_DETAILS {
            runner.push(FakeResponse::ok(
                "Device AA:BB:CC:DD:EE:01 (public)\n\tName: Gamepad\n\tPaired: yes\n\tBonded: yes\n\tTrusted: no\n\tConnected: yes\n\tRSSI: -61\n\tIcon: input-gaming\n",
            ));
        }

        let list = service.devices(Some(1000)).await.unwrap();
        assert_eq!(list.limit, MAX_DEVICES);
        assert_eq!(list.count, MAX_DEVICES);
        assert_eq!(list.total, MAX_DEVICES + 10);
        assert!(list.truncated);
        assert_eq!(list.devices[0].paired, true);
        assert_eq!(list.devices[0].trusted, false);
        assert_eq!(list.devices[0].connected, true);
        assert_eq!(list.devices[0].rssi, Some(-61));
        assert_eq!(list.devices[0].icon.as_deref(), Some("input-gaming"));
    }

    #[tokio::test]
    async fn scan_is_clamped_and_stops_discovery() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(list_output())); // scan preflight
        runner.push(FakeResponse::ok(list_output())); // --timeout scan on
        runner.push(FakeResponse::ok(list_output())); // post-scan list
        runner.push(FakeResponse::ok(&device_line(1))); // devices
        runner.push(FakeResponse::ok(
            "Device AA:BB:CC:DD:EE:01 (public)\n\tName: Device 1\n\tRSSI: -55\n",
        )); // info
        runner.push(FakeResponse::ok("")); // scan off

        let result = service.scan(Some(99)).await.unwrap();
        assert_eq!(result.seconds, MAX_SCAN_SECONDS);
        assert_eq!(result.cleanup, "scan-off");
        let args = runner.args();
        assert!(args.contains(&vec![
            "--timeout".to_owned(),
            MAX_SCAN_SECONDS.to_string(),
            "scan".to_owned(),
            "on".to_owned(),
        ]));
        assert_eq!(
            args.last(),
            Some(&vec!["scan".to_owned(), "off".to_owned()]),
            "scan off is always the final cleanup call"
        );
        assert_eq!(result.devices[0].rssi, Some(-55));
    }

    #[tokio::test]
    async fn scan_timeout_still_stops_discovery() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(list_output())); // preflight
        runner.push(FakeResponse::timeout()); // scan command
        runner.push(FakeResponse::ok("")); // scan off

        let error = service.scan(Some(2)).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.timeout");
        let args = runner.args();
        assert_eq!(
            args.last(),
            Some(&vec!["scan".to_owned(), "off".to_owned()])
        );
    }

    #[tokio::test]
    async fn concurrent_scan_is_busy() {
        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(list_output()));
        let guard = service.inner.scan_lock.try_lock().unwrap();
        let error = service.scan(Some(1)).await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.busy");
        drop(guard);
    }

    #[tokio::test]
    async fn missing_binary_and_no_adapter_are_typed_503() {
        let runner = Arc::new(FakeRunner::new(false));
        let service = service_for(runner.clone());
        let error = service.status().await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.unavailable");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);

        let runner = Arc::new(FakeRunner::new(true));
        let service = service_for(runner.clone());
        runner.push(FakeResponse::ok(""));
        let error = service.status().await.unwrap_err();
        assert_eq!(error.code(), "bluetooth.unavailable");
    }

    #[tokio::test]
    async fn router_with_no_service_returns_typed_not_configured() {
        let app = router(None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/bluetooth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "bluetooth.not-configured");
        assert_eq!(body["configured"], false);
    }

    #[tokio::test]
    async fn router_preserves_typed_errors() {
        let runner = Arc::new(FakeRunner::new(false));
        let app = router(Some(service_for(runner)));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/bluetooth/forget")
                    .header("content-type", "application/json")
                    .body(Body::from(format!(r#"{{"address":"{ADDRESS}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "bluetooth.unavailable");
        assert_eq!(body["supported"], true);
    }

    #[test]
    fn error_detail_truncation_respects_utf8_boundaries() {
        let text = "é".repeat(MAX_ERROR_DETAIL + 100);
        let detail = detail(&text);
        assert!(detail.ends_with('…'));
        assert!(detail.chars().count() <= MAX_ERROR_DETAIL + 1);
    }

    #[test]
    fn ansi_and_scan_logs_are_parsed() {
        let log = "\u{1b}[0;92mNEW\u{1b}[0m] Device 7E:AA:95:AA:A1:DF 7E-AA-95-AA-A1-DF\n\
                   \u{1b}[0;93mCHG\u{1b}[0m] Device 7E:AA:95:AA:A1:DF RSSI: 0xffffffac (-84)\n";
        let seen = parse_scan_log(log);
        assert_eq!(seen.get("7E:AA:95:AA:A1:DF"), Some(&-84));
    }

    #[test]
    fn output_parsers_are_bounded_and_tolerate_missing_names() {
        let devices =
            parse_device_list("Device AA:BB:CC:DD:EE:01\nDevice AA:BB:CC:DD:EE:01 Duplicate\n");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "");

        let adapters = parse_adapter_list(
            &(0..(MAX_ADAPTERS + 5))
                .map(|index| format!("Controller AA:BB:CC:DD:EE:{index:02X} A{index}\n"))
                .collect::<String>(),
        );
        assert_eq!(adapters.len(), MAX_ADAPTERS);
    }

    /// Live status + a two-second scan. It skips on hosts without hci0 so a
    /// container/CI job does not fail or fabricate an adapter. It never pairs,
    /// connects, trusts, or forgets a device.
    #[tokio::test]
    async fn live_status_and_bounded_scan_when_hci0_is_present() {
        if !Path::new("/sys/class/bluetooth/hci0").exists() {
            eprintln!("skipping live Bluetooth test: /sys/class/bluetooth/hci0 is absent");
            return;
        }
        let service = BluetoothService::new(BluetoothConfig::default());
        let status = match service.status().await {
            Ok(status) => status,
            Err(error) => {
                if error.code() == "bluetooth.unavailable" {
                    eprintln!("skipping live Bluetooth test: {error}");
                    return;
                }
                panic!("live Bluetooth status failed: {error}");
            }
        };
        assert!(status.available);
        assert!(!status.adapters.is_empty());
        assert_eq!(status.scan.max_seconds, MAX_SCAN_SECONDS);

        let scan = service
            .scan(Some(2))
            .await
            .expect("live bounded Bluetooth scan failed");
        assert_eq!(scan.seconds, 2);
        assert_eq!(scan.cleanup, "scan-off");
        assert!(scan.count <= MAX_DEVICES);
        // Best-effort cleanup: BlueZ may stop discovery asynchronously after the
        // one-shot scan process exits. This explicit call is the documented
        // cleanup path and is also exercised by the fake-runner tests.
        let _ = service
            .run(&["scan", "off"], Duration::from_secs(3), 8 * 1024)
            .await;
    }
}
