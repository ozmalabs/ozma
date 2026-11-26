// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Read-only Linux 1-Wire sysfs surface.
//!
//! The only source is the operator-supplied sysfs root (normally
//! `/sys/bus/w1/devices`). Device ids are validated as `FF-IIIIIIIIIIII`
//! (two hex family digits, a hyphen, twelve hex id digits) and only joined to
//! the root; no user-controlled path component, symlink, or `..` can escape it.
//! Reads are bounded and returned as sanitized raw lines, with CRC state and a
//! parsed temperature when the `w1_slave` file provides them.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Serialize;

use super::{
    invalid_request, io_failed, json_response, unavailable, BusError, BusState, MAX_ERROR_DETAIL,
};

/// Largest `w1_slave` read.
pub const MAX_W1_SLAVE_BYTES: usize = 4096;
/// Largest `name` sysfs read.
const MAX_NAME_BYTES: usize = 256;
/// Largest number of raw lines returned.
const MAX_RAW_LINES: usize = 8;
/// Largest number of characters per raw line.
const MAX_RAW_LINE_CHARS: usize = 256;
/// Hard cap on directory entries inspected for one listing.
const MAX_SCAN_ENTRIES: usize = 4096;
/// Largest emitted sensor `name`.
const MAX_NAME_CHARS: usize = 64;

/// Validated 1-Wire sysfs configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneWireConfig {
    root: PathBuf,
}

impl OneWireConfig {
    /// Build a configuration for an explicit absolute sysfs root.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, BusError> {
        let config = Self { root: root.into() };
        config.validate()?;
        Ok(config)
    }

    /// Validate the root path shape. Existence is reported by the routes.
    pub fn validate(&self) -> Result<(), BusError> {
        super::validate_absolute_path(&self.root, "1-Wire sysfs root")
    }

    /// Configured sysfs root.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// One entry in `GET /buses/1wire/devices`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OneWireSummary {
    /// Canonical lower-case `family-id` string.
    pub id: String,
    /// Two lower-case hex family digits.
    pub family: String,
    /// Kernel `name` file, when present and readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `GET /buses/1wire/devices` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OneWireDevices {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub available: bool,
    pub root: String,
    pub count: usize,
    /// Valid device directories found, before the response bound.
    pub matched: usize,
    pub limit: usize,
    pub truncated: bool,
    pub devices: Vec<OneWireSummary>,
}

/// `GET /buses/1wire/{id}` response.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OneWireDevice {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    /// The device directory exists.
    pub available: bool,
    pub id: String,
    pub family: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `w1_slave` exists and was read.
    pub w1_slave_available: bool,
    /// `YES`/`NO` from the kernel CRC line, or `null` when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crc_valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_millicelsius: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c: Option<f64>,
    /// Sanitized, bounded lines from `w1_slave`.
    pub raw_lines: Vec<String>,
    /// True when the source or emitted raw lines were truncated by bounds.
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct W1Sample {
    crc_valid: Option<bool>,
    temperature_millicelsius: Option<i32>,
}

/// Internal listing used by the status summary.
pub(super) struct OneWireListing {
    pub summary: Vec<OneWireSummary>,
    pub matched: usize,
    pub truncated: bool,
}

/// Read-only 1-Wire sysfs accessor.
pub(super) struct OneWire {
    config: OneWireConfig,
}

impl OneWire {
    pub(super) fn new(config: OneWireConfig) -> Self {
        Self { config }
    }

    pub(super) fn root(&self) -> &Path {
        self.config.root()
    }

    /// List valid device directories, bounded by `limit`.
    pub(super) fn listing(&self, limit: usize) -> Result<OneWireListing, BusError> {
        let entries = fs::read_dir(self.root()).map_err(|error| {
            unavailable(format!(
                "could not read 1-Wire root {}: {error}",
                self.root().display()
            ))
        })?;
        let mut ids = Vec::new();
        let mut scan_truncated = false;
        for (scanned, entry) in entries.enumerate() {
            if scanned >= MAX_SCAN_ENTRIES {
                scan_truncated = true;
                break;
            }
            let Ok(entry) = entry else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(id) = validate_device_id(&name) else {
                continue;
            };
            // `/sys/bus/.../devices/<id>` is normally a kernel symlink to the
            // real device directory. The basename was validated above, so
            // following that kernel-owned link cannot introduce a user path.
            let path = entry.path();
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_dir() {
                continue;
            }
            ids.push(id);
        }
        ids.sort_unstable();
        ids.dedup();
        let matched = ids.len();
        let truncated = scan_truncated || matched > limit;
        ids.truncate(limit);
        let devices = ids
            .into_iter()
            .map(|id| {
                let family = id[..2].to_owned();
                let name = read_name(&self.root().join(&id));
                OneWireSummary { id, family, name }
            })
            .collect();
        Ok(OneWireListing {
            summary: devices,
            matched,
            truncated,
        })
    }

    /// Read one validated device.
    pub(super) fn read_device(
        &self,
        raw_id: &str,
        max_slave_bytes: usize,
    ) -> Result<OneWireDevice, BusError> {
        let id = validate_device_id(raw_id)?;
        let family = id[..2].to_owned();
        let dir = self.root().join(&id);
        match fs::metadata(&dir) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(unavailable(format!(
                    "1-Wire device {id} does not name a directory"
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(unavailable(format!(
                    "1-Wire device {id} is not present under {}",
                    self.root().display()
                )))
            }
            Err(error) => {
                return Err(io_failed(format!(
                    "could not stat 1-Wire device {id}: {error}"
                )))
            }
        }
        let name = read_name(&dir);
        let slave = dir.join("w1_slave");
        let bounded = max_slave_bytes.min(MAX_W1_SLAVE_BYTES);
        let (text, mut truncated, w1_slave_available) = match read_text_bounded(&slave, bounded) {
            Ok((text, truncated)) => (text, truncated, true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (String::new(), false, false)
            }
            Err(error) => {
                return Err(io_failed(format!(
                    "could not read {}/w1_slave: {error}",
                    id
                )))
            }
        };
        let (raw_lines, line_truncated) = bounded_raw_lines(&text);
        truncated |= line_truncated;
        let sample = parse_w1_slave(&text);
        Ok(OneWireDevice {
            ok: true,
            supported: true,
            configured: true,
            available: true,
            id,
            family,
            name,
            w1_slave_available,
            crc_valid: sample.crc_valid,
            temperature_millicelsius: sample.temperature_millicelsius,
            temperature_c: sample
                .temperature_millicelsius
                .map(|millis| millis as f64 / 1000.0),
            raw_lines,
            truncated,
        })
    }
}

/// Validate and canonicalize a 1-Wire device id.
///
/// Accepted form is exactly `FF-IIIIIIIIIIII` (15 ASCII characters): two hex
/// family digits, `-`, then twelve hex id digits. Uppercase input is lowercased.
/// This is the only place a user-supplied id becomes a path component.
pub fn validate_device_id(raw: &str) -> Result<String, BusError> {
    let raw = raw.trim();
    if raw.len() != 15 {
        return Err(invalid_request(
            "1-Wire id must be exactly 15 characters: two hex family digits, '-', and twelve hex id digits",
        ));
    }
    let bytes = raw.as_bytes();
    if bytes[2] != b'-' {
        return Err(invalid_request(
            "1-Wire id must use the form FF-IIIIIIIIIIII",
        ));
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(index, byte)| index == 2 || byte.is_ascii_hexdigit())
    {
        return Err(invalid_request(
            "1-Wire id must contain only hex digits around one '-'",
        ));
    }
    Ok(raw.to_ascii_lowercase())
}

fn read_name(dir: &Path) -> Option<String> {
    let (text, _) = read_text_bounded(&dir.join("name"), MAX_NAME_BYTES).ok()?;
    let name: String = text
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_NAME_CHARS)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn read_text_bounded(path: &Path, max: usize) -> std::io::Result<(String, bool)> {
    let file = fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut buffer)?;
    let truncated = buffer.len() > max;
    buffer.truncate(max);
    Ok((String::from_utf8_lossy(&buffer).into_owned(), truncated))
}

fn bounded_raw_lines(text: &str) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut truncated = false;
    for line in text.lines() {
        if lines.len() >= MAX_RAW_LINES {
            truncated = true;
            break;
        }
        let sanitized: String = line
            .chars()
            .map(|character| {
                if character.is_control() {
                    '?'
                } else {
                    character
                }
            })
            .collect();
        if sanitized.chars().count() > MAX_RAW_LINE_CHARS {
            truncated = true;
        }
        lines.push(sanitized.chars().take(MAX_RAW_LINE_CHARS).collect());
    }
    (lines, truncated)
}

fn parse_w1_slave(text: &str) -> W1Sample {
    let mut sample = W1Sample::default();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for (index, token) in tokens.iter().enumerate() {
            if token.starts_with("crc=") {
                if let Some(next) = tokens.get(index + 1) {
                    if next.eq_ignore_ascii_case("YES") {
                        sample.crc_valid = Some(true);
                    } else if next.eq_ignore_ascii_case("NO") {
                        sample.crc_valid = Some(false);
                    }
                }
            }
            if let Some(value) = token.strip_prefix("t=") {
                if sample.temperature_millicelsius.is_none() {
                    sample.temperature_millicelsius = value.parse::<i32>().ok();
                }
            }
        }
    }
    sample
}

impl BusState {
    pub(super) fn onewire_devices(&self) -> Result<OneWireDevices, BusError> {
        let Some(service) = &self.inner.onewire else {
            return Err(BusError::NotConfigured);
        };
        let limit = self.inner.config.max_devices;
        let listing = service.listing(limit)?;
        Ok(OneWireDevices {
            ok: true,
            supported: true,
            configured: true,
            available: true,
            root: service.root().display().to_string(),
            count: listing.summary.len(),
            matched: listing.matched,
            limit,
            truncated: listing.truncated,
            devices: listing.summary,
        })
    }

    pub(super) fn onewire_device(&self, id: &str) -> Result<OneWireDevice, BusError> {
        let Some(service) = &self.inner.onewire else {
            return Err(BusError::NotConfigured);
        };
        let max = self.inner.config.max_transfer_bytes.min(MAX_W1_SLAVE_BYTES);
        service.read_device(id, max)
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/1wire/devices", get(devices_handler))
        .route("/buses/1wire/:id", get(device_handler))
        .with_state(state)
}

async fn devices_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.onewire_devices()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "1-Wire listing task failed: {}",
            bounded_error(&error.to_string())
        ))),
    }
}

async fn device_handler(State(state): State<BusState>, AxumPath(id): AxumPath<String>) -> Response {
    match tokio::task::spawn_blocking(move || state.onewire_device(&id)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "1-Wire read task failed: {}",
            bounded_error(&error.to_string())
        ))),
    }
}

fn error_response(error: BusError) -> Response {
    axum::response::IntoResponse::into_response(error)
}

fn bounded_error(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::{BusConfig, BusState};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn state(root: &Path) -> BusState {
        let config = BusConfig::new()
            .with_onewire(root)
            .expect("valid onewire config");
        BusState::new(config).expect("valid bus state")
    }

    fn write_slave(root: &Path, id: &str, body: &str) {
        let dir = root.join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("w1_slave"), body).unwrap();
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
        let value = serde_json::from_slice(&bytes).unwrap_or_else(
            |_| serde_json::json!({"unparseable": String::from_utf8_lossy(&bytes)}),
        );
        (status, value)
    }

    #[test]
    fn device_ids_are_strict_and_traversal_safe() {
        assert_eq!(
            validate_device_id("28-000000ABC123").unwrap(),
            "28-000000abc123"
        );
        for bad in [
            "",
            "..",
            "../etc",
            "28-00000000000",
            "28-0000000000000",
            "2-0000000000000",
            "28_000000000000",
            "28-00000000000g",
            "/etc/passwd",
            "28-00000000/000",
            "28-../000000000",
        ] {
            assert!(
                matches!(
                    validate_device_id(bad),
                    Err(BusError::InvalidRequest { .. })
                ),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn w1_slave_parsing_reports_crc_and_temperature() {
        let valid = "50 01 4b 46 7f ff 0c 10 1c : crc=1c YES\n\
                     50 01 4b 46 7f ff 0c 10 1c t=21000\n";
        let sample = parse_w1_slave(valid);
        assert_eq!(sample.crc_valid, Some(true));
        assert_eq!(sample.temperature_millicelsius, Some(21_000));

        let invalid = "50 01 4b 46 7f ff 0c 10 1c : crc=1c NO\n\
                       50 01 4b 46 7f ff 0c 10 1c t=-1250\n";
        let sample = parse_w1_slave(invalid);
        assert_eq!(sample.crc_valid, Some(false));
        assert_eq!(sample.temperature_millicelsius, Some(-1_250));

        let missing = parse_w1_slave("garbage\n");
        assert_eq!(missing, W1Sample::default());
    }

    #[test]
    fn raw_lines_are_sanitized_and_bounded() {
        let long = format!("{}\n{}", "a".repeat(300), "b\n".repeat(20));
        let (lines, truncated) = bounded_raw_lines(&long);
        assert!(truncated);
        assert!(lines.len() <= MAX_RAW_LINES);
        assert!(lines
            .iter()
            .all(|line| line.chars().count() <= MAX_RAW_LINE_CHARS));

        let control = "ok\u{0007}line\n";
        let (lines, truncated) = bounded_raw_lines(control);
        assert!(!truncated);
        assert_eq!(lines, vec!["ok?line".to_owned()]);
    }

    #[tokio::test]
    async fn fixture_listing_is_sorted_filtered_and_bounded() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write_slave(root, "28-000000000002", "crc=aa YES\nt=1000\n");
        write_slave(root, "28-000000000001", "crc=aa YES\nt=2000\n");
        write_slave(root, "10-000000000003", "crc=aa YES\nt=3000\n");
        fs::create_dir_all(root.join("not-a-device")).unwrap();
        fs::create_dir_all(root.join("28-00000000000z")).unwrap();
        fs::write(root.join("28-000000000001/name"), "DS18B20\n").unwrap();

        let config = BusConfig::new()
            .with_onewire(root)
            .unwrap()
            .with_limits(4096, 32, 2);
        let app = router(BusState::new(config).unwrap());

        let (status, body) = get(&app, "/buses/1wire/devices").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["configured"], true);
        assert_eq!(body["count"], 2);
        assert_eq!(body["matched"], 3);
        assert_eq!(body["truncated"], true);
        let devices = body["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0]["id"], "10-000000000003");
        assert_eq!(devices[1]["id"], "28-000000000001");
        assert_eq!(devices[1]["name"], "DS18B20");
        assert_eq!(devices[1]["family"], "28");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sysfs_symlinked_device_directories_are_followed() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let real = temp.path().join("real-device");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("w1_slave"), "crc=aa YES\nt=1234\n").unwrap();
        fs::write(real.join("name"), "DS18B20\n").unwrap();
        let root = temp.path().join("devices");
        fs::create_dir_all(&root).unwrap();
        symlink(&real, root.join("28-000000000001")).unwrap();

        let app = crate::buses::router(state(&root));
        let (status, body) = get(&app, "/buses/1wire/devices").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 1);
        let (status, body) = get(&app, "/buses/1wire/28-000000000001").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["temperature_millicelsius"], 1234);
    }

    #[tokio::test]
    async fn fixture_detail_reports_temperature_crc_and_raw_lines() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write_slave(
            root,
            "28-000000000001",
            "50 01 4b 46 7f ff 0c 10 1c : crc=1c YES\n\
             50 01 4b 46 7f ff 0c 10 1c t=21000\n",
        );
        fs::write(root.join("28-000000000001/name"), "DS18B20\n").unwrap();
        let app = router(state(root));

        let (status, body) = get(&app, "/buses/1wire/28-000000000001").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], "28-000000000001");
        assert_eq!(body["family"], "28");
        assert_eq!(body["name"], "DS18B20");
        assert_eq!(body["available"], true);
        assert_eq!(body["w1_slave_available"], true);
        assert_eq!(body["crc_valid"], true);
        assert_eq!(body["temperature_millicelsius"], 21_000);
        assert_eq!(body["temperature_c"], 21.0);
        assert_eq!(body["raw_lines"].as_array().unwrap().len(), 2);
        assert_eq!(body["truncated"], false);
    }

    #[tokio::test]
    async fn malformed_id_missing_device_and_unreadable_slave_are_typed() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        write_slave(root, "28-000000000001", "crc=aa YES\nt=1000\n");
        fs::create_dir_all(root.join("28-000000000002")).unwrap();
        let app = router(state(root));

        let (status, body) = get(&app, "/buses/1wire/28-00000000000g").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");

        let (status, body) = get(&app, "/buses/1wire/28-000000000009").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");

        // Present device with no w1_slave is honest, not an error.
        let (status, body) = get(&app, "/buses/1wire/28-000000000002").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["w1_slave_available"], false);
        assert_eq!(body["crc_valid"], Value::Null);
        assert_eq!(body["temperature_c"], Value::Null);
    }

    #[tokio::test]
    async fn missing_root_and_unconfigured_state_are_typed() {
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("missing");
        let app = router(state(&missing));

        let (status, body) = get(&app, "/buses/1wire/devices").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");

        let app = router(BusState::default());
        let (status, body) = get(&app, "/buses/1wire/devices").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        assert_eq!(body["configured"], false);
    }

    #[tokio::test]
    async fn status_reports_onewire_without_opening_anything() {
        let temp = TempDir::new().unwrap();
        write_slave(temp.path(), "28-000000000001", "crc=aa YES\nt=1000\n");
        let app = crate::buses::router(state(temp.path()));
        let (status, body) = get(&app, "/buses/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["onewire"]["configured"], true);
        assert_eq!(body["onewire"]["available"], true);
        assert_eq!(body["onewire"]["devices"], 1);
        assert_eq!(body["i2c"]["configured"], false);
        assert_eq!(body["spi"]["configured"], false);
        assert_eq!(body["guarded"], true);
    }
}
