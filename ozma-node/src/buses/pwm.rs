// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, allowlisted Linux PWM sysfs surface.
//!
//! The allowlist is the complete set of exact `CHIP:CHANNEL[:export]` pairs.
//! Reads are safe for any listed channel; `POST .../set` requires the exact
//! pair and writes only `period`, `duty_cycle`, and `enable` in the channel
//! directory. `:export` additionally permits writing the channel number to the
//! chip's `export` file when the channel directory is absent. There is no
//! `unexport` operation at all, so no chip or channel outside the allowlist can
//! be released or removed.
//!
//! Values are nanoseconds and strictly bounded. Writes are ordered so a period
//! shrink cannot be rejected by a larger current duty cycle: disable first when
//! requested, lower duty to zero when necessary, write the period, write the
//! duty, and enable last.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};

use super::{
    invalid_request, io_failed, json_response, unavailable, BusError, BusLimits, BusState,
    MAX_ERROR_DETAIL, MAX_REQUEST_BODY_BYTES,
};

/// Default Linux PWM class root.
pub const DEFAULT_PWM_ROOT: &str = "/sys/class/pwm";
/// Largest accepted PWM period (1000 seconds in nanoseconds).
pub const MAX_PWM_PERIOD_NS: u64 = 1_000_000_000_000;
/// Largest accepted PWM channel number.
pub const MAX_PWM_CHANNEL: u32 = 4096;
/// How long to wait for an exported channel directory to appear.
const EXPORT_WAIT: Duration = Duration::from_millis(500);
/// Largest sysfs value read.
const MAX_VALUE_BYTES: usize = 64;

/// Exact PWM chip/channel allowlist.
#[derive(Clone, Debug)]
pub struct PwmConfig {
    root: PathBuf,
    allow: BTreeMap<String, BTreeMap<u32, bool>>,
}

impl Default for PwmConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from(DEFAULT_PWM_ROOT),
            allow: BTreeMap::new(),
        }
    }
}

impl PwmConfig {
    /// Parse comma/space separated `CHIP:CHANNEL[:export]` entries.
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
        let parts: Vec<&str> = token.split(':').collect();
        if !matches!(parts.len(), 2 | 3) {
            return Err(invalid_request(format!(
                "invalid --pwm-allow entry {token:?}; expected CHIP:CHANNEL[:export]"
            )));
        }
        let chip = parse_chip(parts[0])?;
        let channel = parse_channel(parts[1])?;
        let export = match parts.get(2).map(|value| value.trim()) {
            None | Some("") => false,
            Some("export") => true,
            Some(other) => {
                return Err(invalid_request(format!(
                    "invalid PWM option {other:?} in {token:?}; expected export"
                )))
            }
        };
        let slot = self.allow.entry(chip).or_default();
        *slot.entry(channel).or_default() |= export;
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
        super::validate_absolute_path(&self.root, "PWM sysfs root")?;
        if self.total_channels() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "PWM allowlist contains {} channels, above the {} absolute bound",
                self.total_channels(),
                super::MAX_DEVICES
            )));
        }
        for (chip, channels) in &self.allow {
            if parse_chip(chip)? != *chip {
                return Err(invalid_request(format!(
                    "PWM chip {chip:?} is not normalized"
                )));
            }
            if channels.is_empty() {
                return Err(invalid_request(format!(
                    "PWM chip {chip} has no allowlisted channels"
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

    /// Exact chip/channel allowlist check before any sysfs write.
    pub fn allows(&self, chip: &str, channel: u32) -> bool {
        self.allow
            .get(chip)
            .is_some_and(|channels| channels.contains_key(&channel))
    }

    /// True only when this exact channel was listed with `:export`.
    pub fn export_allowed(&self, chip: &str, channel: u32) -> bool {
        self.allow
            .get(chip)
            .and_then(|channels| channels.get(&channel))
            .copied()
            .unwrap_or(false)
    }

    pub fn chips(&self) -> impl Iterator<Item = &String> {
        self.allow.keys()
    }

    pub fn channels(&self, chip: &str) -> Option<&BTreeMap<u32, bool>> {
        self.allow.get(chip)
    }

    pub fn total_channels(&self) -> usize {
        self.allow.values().map(BTreeMap::len).sum()
    }

    pub fn export_channels(&self) -> usize {
        self.allow
            .values()
            .flat_map(|channels| channels.values())
            .filter(|export| **export)
            .count()
    }
}

/// Normalize and validate a PWM chip name.
///
/// Accepted forms are `pwmchipN`, `/sys/class/pwm/pwmchipN`, and a bare `N`.
pub fn parse_chip(raw: &str) -> Result<String, BusError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid_request("PWM chip is empty"));
    }
    if raw.contains('/') && !raw.starts_with("/sys/class/pwm/") {
        return Err(invalid_request(format!(
            "invalid PWM chip {raw:?}; expected pwmchipN"
        )));
    }
    let name = raw.strip_prefix("/sys/class/pwm/").unwrap_or(raw);
    let digits = match name.strip_prefix("pwmchip") {
        Some(digits) => digits,
        None if name.bytes().all(|byte| byte.is_ascii_digit()) => name,
        None => {
            return Err(invalid_request(format!(
                "invalid PWM chip {raw:?}; expected pwmchipN"
            )))
        }
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_request(format!(
            "invalid PWM chip {raw:?}; expected pwmchipN with a decimal N"
        )));
    }
    let number: u32 = digits.parse().map_err(|_| {
        invalid_request(format!(
            "invalid PWM chip {raw:?}; chip number is out of range"
        ))
    })?;
    Ok(format!("pwmchip{number}"))
}

/// Parse a route/config channel. Decimal or `0x` hex.
pub fn parse_channel(raw: &str) -> Result<u32, BusError> {
    let raw = raw.trim();
    let parsed = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16)
    } else {
        raw.parse::<u32>()
    };
    let channel = parsed.map_err(|_| {
        invalid_request(format!(
            "invalid PWM channel {raw:?}; expected a decimal or 0xHEX number"
        ))
    })?;
    if channel > MAX_PWM_CHANNEL {
        return Err(invalid_request(format!(
            "PWM channel must be between 0 and {MAX_PWM_CHANNEL}"
        )));
    }
    Ok(channel)
}

fn chip_dir(root: &Path, chip: &str) -> PathBuf {
    root.join(chip)
}

fn channel_dir(root: &Path, chip: &str, channel: u32) -> PathBuf {
    chip_dir(root, chip).join(format!("pwm{channel}"))
}

fn read_u64(path: &Path) -> Result<Option<u64>, BusError> {
    use std::io::Read as _;

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
                "could not read {}: {error}",
                path.display()
            )))
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_VALUE_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_failed(format!("could not read {}: {error}", path.display())))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.trim().parse::<u64>().ok())
}

fn read_bool(path: &Path) -> Result<Option<bool>, BusError> {
    Ok(read_u64(path)?.map(|value| value != 0))
}

fn write_value(path: &Path, value: impl std::fmt::Display) -> Result<(), BusError> {
    fs::write(path, format!("{value}\n")).map_err(|error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            BusError::PermissionDenied {
                detail: format!("writing {}: {error}", path.display()),
            }
        } else if error.kind() == std::io::ErrorKind::NotFound {
            unavailable(format!("{} disappeared: {error}", path.display()))
        } else {
            io_failed(format!("could not write {}: {error}", path.display()))
        }
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PwmValues {
    period_ns: u64,
    duty_ns: u64,
    enabled: bool,
}

fn read_values(dir: &Path) -> Result<Option<PwmValues>, BusError> {
    let Some(period_ns) = read_u64(&dir.join("period"))? else {
        return Ok(None);
    };
    let Some(duty_ns) = read_u64(&dir.join("duty_cycle"))? else {
        return Ok(None);
    };
    let enabled = read_bool(&dir.join("enable"))?.unwrap_or(false);
    Ok(Some(PwmValues {
        period_ns,
        duty_ns,
        enabled,
    }))
}

/// One channel in `GET /buses/pwm`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PwmChannelSummary {
    pub chip: String,
    pub channel: u32,
    pub export_allowed: bool,
    pub exported: bool,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub period_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duty_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `GET /buses/pwm` response. It stats the chip directory and reads only
/// already-exported channel values; it never exports anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PwmListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub channels: Vec<PwmChannelSummary>,
    pub limits: BusLimits,
}

/// `GET /buses/pwm/{chip}/{channel}` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PwmDetailResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub available: bool,
    pub chip: String,
    pub channel: u32,
    pub export_allowed: bool,
    pub exported: bool,
    pub period_ns: u64,
    pub duty_ns: u64,
    pub enabled: bool,
}

/// `POST /buses/pwm/{chip}/{channel}/set` body.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PwmSetRequest {
    #[serde(default)]
    pub period_ns: Option<u64>,
    #[serde(default)]
    pub duty_ns: Option<u64>,
    #[serde(default)]
    pub enable: Option<bool>,
}

/// `POST /buses/pwm/{chip}/{channel}/set` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PwmSetResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub chip: String,
    pub channel: u32,
    pub exported_now: bool,
    pub period_ns: u64,
    pub duty_ns: u64,
    pub enabled: bool,
    pub changed: Vec<String>,
}

fn channel_summary(
    root: &Path,
    chip: &str,
    channel: u32,
    export_allowed: bool,
) -> PwmChannelSummary {
    let chip_present = chip_dir(root, chip).is_dir();
    let dir = channel_dir(root, chip, channel);
    let exported = dir.is_dir();
    let values = if exported {
        read_values(&dir).ok().flatten()
    } else {
        None
    };
    PwmChannelSummary {
        chip: chip.to_owned(),
        channel,
        export_allowed,
        exported,
        available: chip_present,
        period_ns: values.map(|values| values.period_ns),
        duty_ns: values.map(|values| values.duty_ns),
        enabled: values.map(|values| values.enabled),
    }
}

impl BusState {
    pub(super) fn pwm_list(&self) -> Result<PwmListResponse, BusError> {
        if !self.inner.config.pwm.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let root = self.inner.config.pwm.root();
        let limit = self.inner.config.max_devices;
        let mut channels = Vec::new();
        for chip in self.inner.config.pwm.chips() {
            for (channel, export) in self.inner.config.pwm.channels(chip).into_iter().flatten() {
                channels.push(channel_summary(root, chip, *channel, *export));
            }
        }
        let matched = channels.len();
        let truncated = matched > limit;
        channels.truncate(limit);
        Ok(PwmListResponse {
            ok: true,
            supported: true,
            configured: true,
            count: channels.len(),
            limit,
            truncated,
            channels,
            limits: self.limits(),
        })
    }

    pub(super) fn pwm_detail(
        &self,
        chip_raw: &str,
        channel_raw: &str,
    ) -> Result<PwmDetailResponse, BusError> {
        if !self.inner.config.pwm.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let chip = parse_chip(chip_raw)?;
        let channel = parse_channel(channel_raw)?;
        if !self.inner.config.pwm.allows(&chip, channel) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "PWM {chip} channel {channel} is not allowlisted; add --pwm-allow {chip}:{channel}"
                ),
            });
        }
        let root = self.inner.config.pwm.root();
        if !chip_dir(root, &chip).is_dir() {
            return Err(unavailable(format!(
                "PWM chip {chip} is not present under {}",
                root.display()
            )));
        }
        let dir = channel_dir(root, &chip, channel);
        let Some(values) = read_values(&dir)? else {
            return Err(unavailable(format!(
                "PWM {chip} channel {channel} is not exported; add :export to --pwm-allow to allow exporting it"
            )));
        };
        Ok(PwmDetailResponse {
            ok: true,
            supported: true,
            configured: true,
            available: true,
            chip: chip.clone(),
            channel,
            export_allowed: self.inner.config.pwm.export_allowed(&chip, channel),
            exported: true,
            period_ns: values.period_ns,
            duty_ns: values.duty_ns,
            enabled: values.enabled,
        })
    }

    pub(super) fn pwm_set(
        &self,
        chip_raw: &str,
        channel_raw: &str,
        request: PwmSetRequest,
    ) -> Result<PwmSetResponse, BusError> {
        if !self.inner.config.pwm.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let chip = parse_chip(chip_raw)?;
        let channel = parse_channel(channel_raw)?;
        if !self.inner.config.pwm.allows(&chip, channel) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "PWM {chip} channel {channel} is not allowlisted; add --pwm-allow {chip}:{channel}:export to write"
                ),
            });
        }
        if request.period_ns.is_none() && request.duty_ns.is_none() && request.enable.is_none() {
            return Err(invalid_request(
                "PWM set requires at least one of period_ns, duty_ns, or enable",
            ));
        }
        if let Some(period_ns) = request.period_ns {
            if !(1..=MAX_PWM_PERIOD_NS).contains(&period_ns) {
                return Err(invalid_request(format!(
                    "PWM period_ns must be between 1 and {MAX_PWM_PERIOD_NS}"
                )));
            }
        }
        if let Some(duty_ns) = request.duty_ns {
            if duty_ns > MAX_PWM_PERIOD_NS {
                return Err(invalid_request(format!(
                    "PWM duty_ns must be between 0 and {MAX_PWM_PERIOD_NS}"
                )));
            }
        }

        let root = self.inner.config.pwm.root();
        let chip_path = chip_dir(root, &chip);
        if !chip_path.is_dir() {
            return Err(unavailable(format!(
                "PWM chip {chip} is not present under {}",
                root.display()
            )));
        }
        let dir = channel_dir(root, &chip, channel);
        let mut exported_now = false;
        if !dir.is_dir() {
            if !self.inner.config.pwm.export_allowed(&chip, channel) {
                return Err(BusError::NotAllowed {
                    detail: format!(
                        "PWM {chip} channel {channel} is not exported and the allowlist lacks :export; add --pwm-allow {chip}:{channel}:export"
                    ),
                });
            }
            write_value(&chip_path.join("export"), channel)?;
            let deadline = Instant::now() + EXPORT_WAIT;
            while !dir.is_dir() {
                if Instant::now() >= deadline {
                    return Err(io_failed(format!(
                        "PWM {chip} channel {channel} did not appear after export"
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            exported_now = true;
        }

        let current = read_values(&dir)?.ok_or_else(|| {
            unavailable(format!(
                "PWM {chip} channel {channel} directory is missing period/duty_cycle"
            ))
        })?;
        let target_period = request.period_ns.unwrap_or(current.period_ns);
        let target_duty = request.duty_ns.unwrap_or(current.duty_ns);
        if target_duty > target_period {
            return Err(invalid_request(format!(
                "PWM duty_ns {target_duty} exceeds period_ns {target_period}"
            )));
        }

        let mut changed = Vec::new();
        if request.enable == Some(false) {
            write_value(&dir.join("enable"), 0)?;
            changed.push("enable".to_owned());
        }

        let period_changed = target_period != current.period_ns;
        let duty_changed = target_duty != current.duty_ns;
        if period_changed && target_period < current.duty_ns {
            // The kernel rejects a period below the current duty cycle. Drop
            // duty to zero first; the final duty is written below.
            write_value(&dir.join("duty_cycle"), 0)?;
        }
        if period_changed {
            write_value(&dir.join("period"), target_period)?;
            changed.push("period_ns".to_owned());
        }
        if duty_changed || (period_changed && target_period < current.duty_ns) {
            write_value(&dir.join("duty_cycle"), target_duty)?;
            changed.push("duty_ns".to_owned());
        }
        if request.enable == Some(true) {
            write_value(&dir.join("enable"), 1)?;
            changed.push("enable".to_owned());
        }

        let values = read_values(&dir)?.ok_or_else(|| {
            unavailable(format!(
                "PWM {chip} channel {channel} disappeared after the set"
            ))
        })?;
        Ok(PwmSetResponse {
            ok: true,
            supported: true,
            configured: true,
            chip,
            channel,
            exported_now,
            period_ns: values.period_ns,
            duty_ns: values.duty_ns,
            enabled: values.enabled,
            changed,
        })
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/pwm", get(list_handler))
        .route("/buses/pwm/:chip/:channel", get(detail_handler))
        .route(
            "/buses/pwm/:chip/:channel/set",
            post(set_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.pwm_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "PWM list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn detail_handler(
    State(state): State<BusState>,
    AxumPath((chip, channel)): AxumPath<(String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.pwm_detail(&chip, &channel)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "PWM detail task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn set_handler(
    State(state): State<BusState>,
    AxumPath((chip, channel)): AxumPath<(String, String)>,
    body: Result<axum::Json<PwmSetRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.pwm_set(&chip, &channel, request)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "PWM set task failed: {}",
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
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn state_at(root: &Path, entries: &[&str]) -> BusState {
        let config = BusConfig::new()
            .with_pwm_root(root)
            .unwrap()
            .with_pwm_allow(
                &entries
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("valid PWM allowlist");
        BusState::new(config).expect("valid bus state")
    }

    fn exported_channel(root: &Path, chip: &str, channel: u32) -> PathBuf {
        let dir = root.join(chip).join(format!("pwm{channel}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("period"), "1000000\n").unwrap();
        fs::write(dir.join("duty_cycle"), "250000\n").unwrap();
        fs::write(dir.join("enable"), "1\n").unwrap();
        dir
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
    fn allowlist_is_exact_and_traversal_safe() {
        let config =
            PwmConfig::from_entries(["pwmchip0:0:export, /sys/class/pwm/pwmchip1:2", "3:4"])
                .unwrap();
        assert!(config.allows("pwmchip0", 0));
        assert!(config.export_allowed("pwmchip0", 0));
        assert!(config.allows("pwmchip1", 2));
        assert!(!config.export_allowed("pwmchip1", 2));
        assert!(config.allows("pwmchip3", 4));
        assert_eq!(config.total_channels(), 3);
        assert_eq!(config.export_channels(), 1);

        for bad in [
            "pwmchip0",
            "pwmchip0:",
            "pwmchip0:0:unexport",
            "pwmchip0:0:export:extra",
            "../pwmchip0:0",
            "/tmp/pwmchip0:0",
            "pwmchipX:0",
            "pwmchip0:../0",
            "pwmchip0:4097",
            "foo:0",
        ] {
            assert!(
                PwmConfig::from_entries([bad]).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn chip_and_channel_segments_normalize() {
        for raw in ["pwmchip0", "/sys/class/pwm/pwmchip0", "0"] {
            assert_eq!(parse_chip(raw).unwrap(), "pwmchip0");
        }
        for bad in ["", "pwmchip", "pwmchipX", "../pwmchip0", "/dev/pwmchip0"] {
            assert!(parse_chip(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_channel("3").unwrap(), 3);
        assert_eq!(parse_channel("0x10").unwrap(), 16);
        assert!(parse_channel("-1").is_err());
        assert!(parse_channel("4097").is_err());
    }

    #[tokio::test]
    async fn fixture_detail_and_set_write_exact_sysfs_values() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let dir = exported_channel(root, "pwmchip0", 0);
        // A sentinel file proves the module never touches unexport.
        fs::write(root.join("pwmchip0/unexport"), "sentinel\n").unwrap();
        let app = router(state_at(root, &["pwmchip0:0"]));

        let (status, body) = get(&app, "/buses/pwm").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["count"], 1);
        assert_eq!(body["channels"][0]["exported"], true);
        assert_eq!(body["channels"][0]["period_ns"], 1_000_000);
        assert_eq!(body["channels"][0]["duty_ns"], 250_000);
        assert_eq!(body["channels"][0]["enabled"], true);

        let (status, body) = get(&app, "/buses/pwm/pwmchip0/0").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["period_ns"], 1_000_000);
        assert_eq!(body["duty_ns"], 250_000);
        assert_eq!(body["enabled"], true);
        assert_eq!(body["export_allowed"], false);

        let (status, body) = post(
            &app,
            "/buses/pwm/pwmchip0/0/set",
            json!({"period_ns": 2_000_000, "duty_ns": 500_000, "enable": false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["period_ns"], 2_000_000);
        assert_eq!(body["duty_ns"], 500_000);
        assert_eq!(body["enabled"], false);
        assert!(body["changed"]
            .as_array()
            .unwrap()
            .contains(&json!("period_ns")));
        assert_eq!(
            fs::read_to_string(dir.join("period")).unwrap().trim(),
            "2000000"
        );
        assert_eq!(
            fs::read_to_string(dir.join("duty_cycle")).unwrap().trim(),
            "500000"
        );
        assert_eq!(fs::read_to_string(dir.join("enable")).unwrap().trim(), "0");
        assert_eq!(
            fs::read_to_string(root.join("pwmchip0/unexport"))
                .unwrap()
                .trim(),
            "sentinel",
            "no unexport write may occur"
        );

        // Shrinking the period below the current duty must still succeed and
        // end at the requested exact values.
        let (status, body) = post(
            &app,
            "/buses/pwm/pwmchip0/0/set",
            json!({"period_ns": 300_000, "duty_ns": 100_000, "enable": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["period_ns"], 300_000);
        assert_eq!(body["duty_ns"], 100_000);
        assert_eq!(body["enabled"], true);
        assert_eq!(
            fs::read_to_string(dir.join("period")).unwrap().trim(),
            "300000"
        );
    }

    #[tokio::test]
    async fn set_bounds_and_allowlists_are_typed_before_writing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let dir = exported_channel(root, "pwmchip0", 0);
        fs::create_dir_all(root.join("pwmchip1")).unwrap();
        let app = router(state_at(root, &["pwmchip0:0"]));

        for body in [
            json!({}),
            json!({"period_ns": 0}),
            json!({"duty_ns": 2_000_000}),
            json!({"period_ns": 1000, "duty_ns": 2000}),
            json!({"period_ns": MAX_PWM_PERIOD_NS + 1}),
            json!({"duty_ns": MAX_PWM_PERIOD_NS + 1}),
            json!({"enable": true, "unknown": 1}),
        ] {
            let (status, response) = post(&app, "/buses/pwm/pwmchip0/0/set", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["code"], "bus.invalid-request");
        }
        assert_eq!(
            fs::read_to_string(dir.join("period")).unwrap().trim(),
            "1000000"
        );

        for uri in [
            "/buses/pwm/pwmchip1/0/set",
            "/buses/pwm/pwmchip0/1/set",
            "/buses/pwm/pwmchip9/0/set",
        ] {
            let (status, response) = post(&app, uri, json!({"enable": true})).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(response["code"], "bus.not-allowed", "{uri}");
        }

        let (status, body) = get(&app, "/buses/pwm/pwmchip9/0").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");

        let app = router(state_at(root, &["pwmchip1:0"]));
        let (status, body) = get(&app, "/buses/pwm/pwmchip1/0").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.unavailable");

        // Missing channel without :export is read-only, never auto-exported.
        let (status, body) = post(
            &app,
            "/buses/pwm/pwmchip1/0/set",
            json!({"period_ns": 1000, "duty_ns": 500}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
        assert!(!root.join("pwmchip1/pwm0").exists());
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed_and_does_not_write() {
        let app = router(BusState::default());
        for uri in ["/buses/pwm", "/buses/pwm/pwmchip0/0"] {
            let (status, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["code"], "bus.not-configured", "{uri}");
        }
        let (status, body) = post(&app, "/buses/pwm/pwmchip0/0/set", json!({"enable": true})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
    }

    /// Live typed evidence on a host without PWM controllers. Ignored by
    /// default; it only reads the kernel class root and never loads a module.
    #[test]
    #[ignore = "requires a host with no /sys/class/pwm; asserts typed unavailable"]
    fn live_pwm_root_is_typed_unavailable() {
        let state = state_at(Path::new("/sys/class/pwm"), &["pwmchip0:0"]);
        let error = state
            .pwm_detail("pwmchip0", "0")
            .expect_err("no PWM chip exists on this host");
        assert!(
            matches!(error, BusError::Unavailable { .. }),
            "expected typed unavailable, got {error:?}"
        );
    }
}
