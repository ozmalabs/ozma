// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Read-only, allowlisted Linux IIO sysfs surface.
//!
//! The allowlist is the complete set of exact `DEVICE:CHANNEL` pairs, for
//! example `iio:device0:voltage0`. Nothing is enumerated or probed by default:
//! listing and detail routes only read the configured sysfs root, and there is
//! no write, trigger, buffer, or event operation. Device names are normalized
//! to `iio:deviceN`; channel names are restricted to a small identifier
//! alphabet so no user-controlled path component can escape the root.
//!
//! Files read per channel are `in_CHANNEL_raw`, `in_CHANNEL_scale`,
//! `in_CHANNEL_offset`, `in_CHANNEL_input`, `in_CHANNEL_type`, and
//! `in_CHANNEL_sampling_frequency` (falling back to the device
//! `sampling_frequency`). Every read is byte-bounded. Missing optional files
//! are reported as `null`; a channel with neither `_raw` nor `_input` is
//! reported as unavailable rather than inventing a reading.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Serialize;

use super::{
    invalid_request, io_failed, json_response, unavailable, BusError, BusLimits, BusState,
    MAX_ERROR_DETAIL,
};

/// Default Linux IIO sysfs root.
pub const DEFAULT_IIO_ROOT: &str = "/sys/bus/iio/devices";
/// Largest accepted sysfs value read.
const MAX_VALUE_BYTES: usize = 256;
/// Largest emitted device/channel/type string.
const MAX_NAME_CHARS: usize = 64;

/// Exact IIO device/channel allowlist.
#[derive(Clone, Debug)]
pub struct IioConfig {
    root: PathBuf,
    allow: BTreeMap<String, BTreeSet<String>>,
}

impl Default for IioConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from(DEFAULT_IIO_ROOT),
            allow: BTreeMap::new(),
        }
    }
}

impl IioConfig {
    /// Parse comma/space separated `DEVICE:CHANNEL` entries.
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
                config.add_entry(token)?;
            }
        }
        config.validate()?;
        Ok(config)
    }

    fn add_entry(&mut self, token: &str) -> Result<(), BusError> {
        let Some((device, channel)) = token.rsplit_once(':') else {
            return Err(invalid_request(format!(
                "invalid --iio-allow entry {token:?}; expected DEVICE:CHANNEL"
            )));
        };
        let device = parse_device(device)?;
        let channel = validate_channel(channel)?;
        self.allow.entry(device).or_default().insert(channel);
        Ok(())
    }

    /// Replace the sysfs root. Existence is reported by the routes.
    pub fn with_root(mut self, root: impl Into<PathBuf>) -> Result<Self, BusError> {
        self.root = root.into();
        self.validate()?;
        Ok(self)
    }

    /// Validate the root path and absolute entry ceiling.
    pub fn validate(&self) -> Result<(), BusError> {
        super::validate_absolute_path(&self.root, "IIO sysfs root")?;
        if self.total_channels() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "IIO allowlist contains {} channels, above the {} absolute bound",
                self.total_channels(),
                super::MAX_DEVICES
            )));
        }
        for (device, channels) in &self.allow {
            if parse_device(device)? != *device {
                return Err(invalid_request(format!(
                    "IIO device {device:?} is not normalized"
                )));
            }
            if channels.is_empty() {
                return Err(invalid_request(format!(
                    "IIO device {device} has no allowlisted channels"
                )));
            }
        }
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.allow.is_empty()
    }

    /// Configured sysfs root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Exact pair allowlist check.
    pub fn allows(&self, device: &str, channel: &str) -> bool {
        self.allow
            .get(device)
            .is_some_and(|channels| channels.contains(channel))
    }

    /// True when the device has at least one allowlisted channel.
    pub fn allows_device(&self, device: &str) -> bool {
        self.allow.get(device).is_some_and(|set| !set.is_empty())
    }

    /// Allowlisted channels for one device, ascending.
    pub fn channels(&self, device: &str) -> Option<&BTreeSet<String>> {
        self.allow.get(device)
    }

    /// Allowlisted devices, ascending.
    pub fn devices(&self) -> impl Iterator<Item = &String> {
        self.allow.keys()
    }

    pub fn total_channels(&self) -> usize {
        self.allow.values().map(BTreeSet::len).sum()
    }
}

/// Normalize and validate an IIO device segment.
///
/// Accepted forms are `iio:deviceN`, `deviceN`, and a bare decimal `N`.
pub fn parse_device(raw: &str) -> Result<String, BusError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid_request("IIO device is empty"));
    }
    if raw.contains('/') || raw.contains('\\') || raw.contains("..") {
        return Err(invalid_request(format!(
            "invalid IIO device {raw:?}; expected iio:deviceN"
        )));
    }
    let digits = match raw.strip_prefix("iio:device") {
        Some(digits) => digits,
        None => raw
            .strip_prefix("device")
            .or_else(|| raw.bytes().all(|byte| byte.is_ascii_digit()).then_some(raw))
            .ok_or_else(|| {
                invalid_request(format!("invalid IIO device {raw:?}; expected iio:deviceN"))
            })?,
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_request(format!(
            "invalid IIO device {raw:?}; expected iio:deviceN with a decimal N"
        )));
    }
    let number: u32 = digits.parse().map_err(|_| {
        invalid_request(format!(
            "invalid IIO device {raw:?}; device number is out of range"
        ))
    })?;
    Ok(format!("iio:device{number}"))
}

/// Validate an IIO channel name as one safe sysfs path component.
pub fn validate_channel(raw: &str) -> Result<String, BusError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid_request("IIO channel is empty"));
    }
    if raw.len() > MAX_NAME_CHARS {
        return Err(invalid_request(format!(
            "IIO channel is longer than {MAX_NAME_CHARS} characters"
        )));
    }
    if raw.starts_with('.')
        || raw.contains('/')
        || raw.contains('\\')
        || raw.contains(':')
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(invalid_request(format!(
            "invalid IIO channel {raw:?}; expected letters, digits, '_' or '-'"
        )));
    }
    Ok(raw.to_owned())
}

fn device_dir(root: &Path, device: &str) -> PathBuf {
    root.join(device)
}

fn channel_prefix(channel: &str) -> String {
    format!("in_{channel}_")
}

/// One channel's sysfs facts.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct IioChannelFacts {
    pub channel: String,
    /// At least one of `_raw`/`_input` was readable.
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<f64>,
    /// The kernel `_input` value when present, otherwise
    /// `(raw + offset) * scale` when raw and scale are both present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_frequency: Option<f64>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<String>,
}

/// One allowlisted device in `GET /buses/iio`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IioDeviceSummary {
    pub device: String,
    pub available: bool,
    pub channels: Vec<String>,
}

/// `GET /buses/iio` response. Only configuration is read plus a stat of each
/// allowlisted device directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IioListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub devices: Vec<IioDeviceSummary>,
    pub limits: BusLimits,
}

/// `GET /buses/iio/{device}` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IioDeviceResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub available: bool,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub channels: Vec<IioChannelFacts>,
}

/// `GET /buses/iio/{device}/{channel}` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IioChannelResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub available: bool,
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_frequency: Option<f64>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<String>,
}

fn read_text_bounded(path: &Path, max: usize) -> Result<Option<String>, BusError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(BusError::PermissionDenied {
                detail: format!("reading {}: {error}", path.display()),
            })
        }
        Err(error) => {
            return Err(io_failed(format!(
                "could not open {}: {error}",
                path.display()
            )))
        }
    };
    let mut buffer = Vec::new();
    file.take(max as u64)
        .read_to_end(&mut buffer)
        .map_err(|error| io_failed(format!("could not read {}: {error}", path.display())))?;
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

fn read_number_i64(dir: &Path, file: &str) -> Result<Option<i64>, BusError> {
    let Some(text) = read_text_bounded(&dir.join(file), MAX_VALUE_BYTES)? else {
        return Ok(None);
    };
    Ok(text.trim().parse::<i64>().ok())
}

fn read_number_f64(dir: &Path, file: &str) -> Result<Option<f64>, BusError> {
    let Some(text) = read_text_bounded(&dir.join(file), MAX_VALUE_BYTES)? else {
        return Ok(None);
    };
    Ok(text.trim().parse::<f64>().ok())
}

fn read_string(dir: &Path, file: &str) -> Result<Option<String>, BusError> {
    let Some(text) = read_text_bounded(&dir.join(file), MAX_VALUE_BYTES)? else {
        return Ok(None);
    };
    let value: String = text
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    Ok((!value.is_empty()).then_some(value))
}

fn read_channel(
    dir: &Path,
    device_sampling: Option<f64>,
    channel: &str,
) -> Result<IioChannelFacts, BusError> {
    let prefix = channel_prefix(channel);
    let raw = read_number_i64(dir, &format!("{prefix}raw"))?;
    let input = read_number_f64(dir, &format!("{prefix}input"))?;
    let scale = read_number_f64(dir, &format!("{prefix}scale"))?;
    let offset = read_number_f64(dir, &format!("{prefix}offset"))?;
    let sampling_frequency =
        read_number_f64(dir, &format!("{prefix}sampling_frequency"))?.or(device_sampling);
    let channel_type = read_string(dir, &format!("{prefix}type"))?;
    let processed = input.or_else(|| {
        raw.and_then(|raw| scale.map(|scale| (raw as f64 + offset.unwrap_or(0.0)) * scale))
    });
    Ok(IioChannelFacts {
        channel: channel.to_owned(),
        available: raw.is_some() || input.is_some(),
        raw,
        scale,
        offset,
        processed,
        sampling_frequency,
        channel_type,
    })
}

fn device_name(dir: &Path) -> Result<Option<String>, BusError> {
    read_string(dir, "name")
}

impl BusState {
    pub(super) fn iio_list(&self) -> Result<IioListResponse, BusError> {
        if !self.inner.config.iio.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let limit = self.inner.config.max_devices;
        let mut devices: Vec<IioDeviceSummary> = self
            .inner
            .config
            .iio
            .devices()
            .map(|device| IioDeviceSummary {
                device: device.clone(),
                available: device_dir(self.inner.config.iio.root(), device).is_dir(),
                channels: self
                    .inner
                    .config
                    .iio
                    .channels(device)
                    .map(|channels| channels.iter().cloned().collect())
                    .unwrap_or_default(),
            })
            .collect();
        let matched = devices.len();
        let truncated = matched > limit;
        devices.truncate(limit);
        Ok(IioListResponse {
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

    pub(super) fn iio_device(&self, device_raw: &str) -> Result<IioDeviceResponse, BusError> {
        if !self.inner.config.iio.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let device = parse_device(device_raw)?;
        let Some(channels) = self.inner.config.iio.channels(&device) else {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "IIO device {device} is not allowlisted; add --iio-allow {device}:CHANNEL"
                ),
            });
        };
        let root = self.inner.config.iio.root();
        let dir = device_dir(root, &device);
        if !dir.is_dir() {
            return Err(unavailable(format!(
                "IIO device {device} is not present under {}",
                root.display()
            )));
        }
        let name = device_name(&dir)?;
        let sampling = read_number_f64(&dir, "sampling_frequency")?;
        let limit = self.inner.config.max_devices;
        let mut facts = Vec::new();
        for channel in channels {
            facts.push(read_channel(&dir, sampling, channel)?);
        }
        let matched = facts.len();
        let truncated = matched > limit;
        facts.truncate(limit);
        Ok(IioDeviceResponse {
            ok: true,
            supported: true,
            configured: true,
            available: true,
            device,
            name,
            count: facts.len(),
            limit,
            truncated,
            channels: facts,
        })
    }

    pub(super) fn iio_channel(
        &self,
        device_raw: &str,
        channel_raw: &str,
    ) -> Result<IioChannelResponse, BusError> {
        if !self.inner.config.iio.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let device = parse_device(device_raw)?;
        let channel = validate_channel(channel_raw)?;
        if !self.inner.config.iio.allows(&device, &channel) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "IIO {device} channel {channel} is not allowlisted; add --iio-allow {device}:{channel}"
                ),
            });
        }
        let root = self.inner.config.iio.root();
        let dir = device_dir(root, &device);
        if !dir.is_dir() {
            return Err(unavailable(format!(
                "IIO device {device} is not present under {}",
                root.display()
            )));
        }
        let name = device_name(&dir)?;
        let sampling = read_number_f64(&dir, "sampling_frequency")?;
        let facts = read_channel(&dir, sampling, &channel)?;
        if !facts.available {
            return Err(unavailable(format!(
                "{device} channel {channel} has no readable raw/input sysfs file under {}",
                dir.display()
            )));
        }
        Ok(IioChannelResponse {
            ok: true,
            supported: true,
            configured: true,
            available: true,
            device,
            name,
            channel: facts.channel,
            raw: facts.raw,
            scale: facts.scale,
            offset: facts.offset,
            processed: facts.processed,
            sampling_frequency: facts.sampling_frequency,
            channel_type: facts.channel_type,
        })
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/iio", get(list_handler))
        .route("/buses/iio/:device", get(device_handler))
        .route("/buses/iio/:device/:channel", get(channel_handler))
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.iio_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "IIO list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn device_handler(
    State(state): State<BusState>,
    AxumPath(device): AxumPath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.iio_device(&device)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "IIO device task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn channel_handler(
    State(state): State<BusState>,
    AxumPath((device, channel)): AxumPath<(String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.iio_channel(&device, &channel)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "IIO channel task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
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
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn state(entries: &[&str]) -> BusState {
        let config = BusConfig::new()
            .with_iio_allow(
                &entries
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("valid IIO allowlist");
        BusState::new(config).expect("valid bus state")
    }

    fn state_at(root: &Path, entries: &[&str]) -> BusState {
        let config = BusConfig::new()
            .with_iio_root(root)
            .unwrap()
            .with_iio_allow(
                &entries
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("valid IIO allowlist");
        BusState::new(config).expect("valid bus state")
    }

    fn write(dir: &Path, name: &str, value: &str) {
        fs::write(dir.join(name), value).unwrap();
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

    #[test]
    fn allowlist_is_exact_and_traversal_safe() {
        let config =
            IioConfig::from_entries(["iio:device0:voltage0, device1:temp1", "0:current2"]).unwrap();
        assert!(config.is_configured());
        assert!(config.allows("iio:device0", "voltage0"));
        assert!(config.allows("iio:device1", "temp1"));
        assert!(config.allows("iio:device0", "current2"));
        assert!(!config.allows("iio:device0", "temp1"));
        assert_eq!(config.total_channels(), 3);

        for bad in [
            "iio:device0",
            "iio:device0:",
            "iio:device0:../voltage0",
            "iio:device0:voltage/0",
            "iio:device0:voltage:0",
            "../../etc:voltage0",
            "iio:deviceX:voltage0",
            "iio:device0:voltage.0",
        ] {
            assert!(
                IioConfig::from_entries([bad]).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn device_and_channel_segments_normalize() {
        for raw in ["iio:device0", "device0", "0"] {
            assert_eq!(parse_device(raw).unwrap(), "iio:device0");
        }
        for bad in [
            "",
            "iio:device",
            "iio:deviceX",
            "../iio:device0",
            "iio:device0/1",
        ] {
            assert!(parse_device(bad).is_err(), "{bad}");
        }
        assert_eq!(validate_channel("voltage0").unwrap(), "voltage0");
        assert_eq!(validate_channel("accel_x").unwrap(), "accel_x");
        for bad in ["", ".", "..", "voltage/0", "voltage:0", "voltage.0"] {
            assert!(validate_channel(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn fixture_listing_detail_and_processed_value() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let device = root.join("iio:device0");
        fs::create_dir_all(&device).unwrap();
        write(&device, "name", "ads1015\n");
        write(&device, "in_voltage0_raw", "2048\n");
        write(&device, "in_voltage0_scale", "0.5\n");
        write(&device, "in_voltage0_offset", "-2\n");
        write(&device, "in_voltage0_type", "le:s16/16>>0\n");
        write(&device, "in_voltage0_sampling_frequency", "1000\n");
        write(&device, "in_temp1_raw", "42\n");
        write(&device, "in_temp1_input", "42000\n");
        write(&device, "in_temp1_scale", "1000\n");
        fs::create_dir_all(root.join("iio:device1")).unwrap();

        let app = router(state_at(
            root,
            &[
                "iio:device0:voltage0",
                "iio:device0:temp1",
                "iio:device1:voltage0",
            ],
        ));

        let (status, body) = get(&app, "/buses/iio").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], true);
        assert_eq!(body["count"], 2);
        assert_eq!(body["devices"][0]["device"], "iio:device0");
        assert_eq!(body["devices"][0]["available"], true);
        assert_eq!(body["devices"][1]["available"], true);

        let (status, body) = get(&app, "/buses/iio/iio:device0").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "ads1015");
        let channels = body["channels"].as_array().unwrap();
        assert_eq!(channels.len(), 2);
        let voltage = channels
            .iter()
            .find(|channel| channel["channel"] == "voltage0")
            .expect("voltage0 listed");
        assert_eq!(voltage["raw"], 2048);
        assert_eq!(voltage["scale"], 0.5);
        assert_eq!(voltage["offset"], -2.0);
        assert_eq!(voltage["processed"], 1023.0);
        assert_eq!(voltage["sampling_frequency"], 1000.0);
        assert_eq!(voltage["type"], "le:s16/16>>0");

        let (status, body) = get(&app, "/buses/iio/iio:device0/voltage0").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["raw"], 2048);
        assert_eq!(body["processed"], 1023.0);

        let (status, body) = get(&app, "/buses/iio/iio:device0/temp1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["processed"], 42000.0, "kernel _input wins");
    }

    #[tokio::test]
    async fn unlisted_missing_and_unreadable_channels_are_typed() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let device = root.join("iio:device0");
        fs::create_dir_all(&device).unwrap();
        write(&device, "in_voltage0_raw", "1\n");
        let app = router(state_at(root, &["iio:device0:voltage0"]));

        let (status, body) = get(&app, "/buses/iio/iio:device0/temp1").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let (status, body) = get(&app, "/buses/iio/iio:device1").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let (status, body) = get(&app, "/buses/iio/iio:device9/voltage0").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let (status, body) = get(&app, "/buses/iio/iio:device0/current2").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let (status, body) = get(&app, "/buses/iio/not-a-device/voltage0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");

        let app = router(state_at(root, &["iio:device9:voltage0"]));
        let (status, body) = get(&app, "/buses/iio/iio:device9/voltage0").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");

        let empty = root.join("iio:device1");
        fs::create_dir_all(&empty).unwrap();
        let app = router(state_at(root, &["iio:device1:voltage0"]));
        let (status, body) = get(&app, "/buses/iio/iio:device1/voltage0").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed_and_does_not_read() {
        let app = router(BusState::default());
        for uri in [
            "/buses/iio",
            "/buses/iio/iio:device0",
            "/buses/iio/iio:device0/voltage0",
        ] {
            let (status, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["code"], "bus.not-configured", "{uri}");
        }
    }

    /// Live typed evidence on a host without IIO devices. This is ignored by
    /// default and deliberately uses the kernel root without loading modules.
    #[test]
    #[ignore = "requires a host with no /sys/bus/iio/devices; asserts typed unavailable"]
    fn live_iio_root_is_typed_unavailable() {
        let state = state(&["iio:device0:voltage0"]);
        let error = state
            .iio_channel("iio:device0", "voltage0")
            .expect_err("no IIO device exists on this host");
        assert!(
            matches!(error, BusError::Unavailable { .. }),
            "expected typed unavailable, got {error:?}"
        );
    }
}
