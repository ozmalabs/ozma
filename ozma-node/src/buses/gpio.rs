// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, allowlisted GPIO character-device surface.
//!
//! The allowlist is the complete set of exact `CHIP:LINE[:input|output]`
//! entries the node may touch. There is no default chip, no line scan, no
//! chip-wide operation, and no implicit line discovery:
//!
//! * a chip name is normalized to `gpiochipN` and opened as `/dev/gpiochipN`;
//! * every route resolves the chip/line and checks the exact pair **before**
//!   opening the chip;
//! * a line defaults to `input`; `set` and `pulse` require an explicit
//!   `:output` entry;
//! * `set`/`pulse` are serialized per line, and a pulse restores the previous
//!   output value even when the caller only asked for a bounded edge.
//!
//! The real backend uses the safe `gpio-cdev` wrapper (Linux GPIO v1 chardev).
//! No `unsafe` is used here. A non-Linux build returns typed `bus.unsupported`.

use std::collections::BTreeMap;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};

use super::{
    invalid_request, io_failed, json_response, unavailable, BusError, BusLimits, BusState,
    MAX_ERROR_DETAIL, MAX_GPIO_PULSE_MS, MAX_REQUEST_BODY_BYTES, MIN_GPIO_PULSE_MS,
};

/// Consumer label presented to the kernel for every requested line.
pub const GPIO_CONSUMER: &str = "ozma-node";

/// Direction recorded in the operator allowlist.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GpioDirection {
    Input,
    Output,
}

impl GpioDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
        }
    }
}

/// Exact GPIO allowlist: chip -> line -> direction.
#[derive(Clone, Debug, Default)]
pub struct GpioConfig {
    allow: BTreeMap<String, BTreeMap<u32, GpioDirection>>,
}

impl GpioConfig {
    /// Parse comma/space separated `CHIP:LINE[:input|output]` entries.
    ///
    /// `CHIP` accepts `gpiochipN`, `/dev/gpiochipN`, or a bare chip number.
    /// Duplicate identical entries collapse; the same line listed with two
    /// different directions is rejected rather than silently widened.
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
                "invalid --gpio-allow entry {token:?}; expected CHIP:LINE[:input|output]"
            )));
        }
        let chip = parse_chip(parts[0])?;
        let line = parse_line(parts[1])?;
        let direction = match parts.get(2).map(|value| value.trim()) {
            None | Some("") | Some("input") => GpioDirection::Input,
            Some("output") => GpioDirection::Output,
            Some(other) => {
                return Err(invalid_request(format!(
                    "invalid GPIO direction {other:?} in {token:?}; expected input or output"
                )))
            }
        };
        let lines = self.allow.entry(chip.clone()).or_default();
        if let Some(existing) = lines.get(&line) {
            if *existing != direction {
                return Err(invalid_request(format!(
                    "{chip} line {line} is allowlisted as {} and {}; use one exact direction",
                    existing.as_str(),
                    direction.as_str()
                )));
            }
        } else {
            lines.insert(line, direction);
        }
        Ok(())
    }

    /// Validate the absolute entry ceiling and non-empty chip maps.
    pub fn validate(&self) -> Result<(), BusError> {
        if self.total_lines() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "GPIO allowlist contains {} lines, above the {} absolute bound",
                self.total_lines(),
                super::MAX_DEVICES
            )));
        }
        for (chip, lines) in &self.allow {
            if parse_chip(chip)? != *chip {
                return Err(invalid_request(format!(
                    "GPIO chip {chip:?} is not normalized"
                )));
            }
            if lines.is_empty() {
                return Err(invalid_request(format!(
                    "GPIO chip {chip} has no allowlisted lines"
                )));
            }
        }
        Ok(())
    }

    /// True when at least one exact chip/line pair is allowlisted.
    pub fn is_configured(&self) -> bool {
        !self.allow.is_empty()
    }

    /// Exact allowlist check used before any chip is opened.
    pub fn allows(&self, chip: &str, line: u32) -> bool {
        self.allow
            .get(chip)
            .is_some_and(|lines| lines.contains_key(&line))
    }

    /// Configured direction for one exact pair.
    pub fn direction(&self, chip: &str, line: u32) -> Option<GpioDirection> {
        self.allow
            .get(chip)
            .and_then(|lines| lines.get(&line))
            .copied()
    }

    /// Allowlisted chips in ascending order.
    pub fn chips(&self) -> impl Iterator<Item = &String> {
        self.allow.keys()
    }

    /// Allowlisted lines for one chip.
    pub fn lines(&self, chip: &str) -> Option<&BTreeMap<u32, GpioDirection>> {
        self.allow.get(chip)
    }

    /// Number of allowlisted chips.
    pub fn len(&self) -> usize {
        self.allow.len()
    }

    /// True when no chip has an allowlisted line.
    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }

    /// Number of exact chip/line pairs.
    pub fn total_lines(&self) -> usize {
        self.allow.values().map(BTreeMap::len).sum()
    }
}

/// Normalize and validate an allowlist/route chip name.
pub fn parse_chip(raw: &str) -> Result<String, BusError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid_request("GPIO chip is empty"));
    }
    let name = raw.strip_prefix("/dev/").unwrap_or(raw);
    let digits = match name.strip_prefix("gpiochip") {
        Some(digits) => digits,
        None if name.bytes().all(|byte| byte.is_ascii_digit()) => name,
        None => {
            return Err(invalid_request(format!(
                "invalid GPIO chip {raw:?}; expected gpiochipN"
            )))
        }
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_request(format!(
            "invalid GPIO chip {raw:?}; expected gpiochipN with a decimal N"
        )));
    }
    let number: u32 = digits.parse().map_err(|_| {
        invalid_request(format!(
            "invalid GPIO chip {raw:?}; chip number is out of range"
        ))
    })?;
    Ok(format!("gpiochip{number}"))
}

/// Parse a route/config line offset. Decimal or `0x` hex.
pub fn parse_line(raw: &str) -> Result<u32, BusError> {
    let raw = raw.trim();
    let parsed = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16)
    } else {
        raw.parse::<u32>()
    };
    parsed.map_err(|_| {
        invalid_request(format!(
            "invalid GPIO line {raw:?}; expected a 32-bit decimal or 0xHEX offset"
        ))
    })
}

/// `/dev/gpiochipN` path for a normalized chip name.
pub(crate) fn device_path(chip: &str) -> String {
    format!("/dev/{chip}")
}

/// Kernel line facts returned by `GET /buses/gpio/{chip}/{line}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GpioLineInfo {
    pub chip: String,
    pub line: u32,
    pub device: String,
    pub available: bool,
    /// Direction requested by the kernel line information (not the allowlist).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_direction: Option<GpioDirection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumer: Option<String>,
    pub kernel: bool,
    pub active_low: bool,
    pub open_drain: bool,
    pub open_source: bool,
    /// Bias is not exposed by the GPIO v1 character-device ABI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bias: Option<String>,
}

/// A successful simple set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpioSetOutcome {
    pub value: u8,
}

/// A successful bounded pulse and its restore evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpioPulseOutcome {
    pub previous: u8,
    pub pulsed: u8,
    pub restored: u8,
}

/// Backend abstraction. Production uses [`RealGpioBackend`]; tests inject a
/// fake so allowlist/direction enforcement is observable before any chip open.
pub trait GpioBackend: Send + Sync {
    fn info(&self, chip: &str, line: u32) -> Result<GpioLineInfo, BusError>;
    /// Read a line by briefly requesting it as an input.
    fn get(&self, chip: &str, line: u32) -> Result<u8, BusError>;
    /// Drive an allowlisted output to `value`.
    fn set(&self, chip: &str, line: u32, value: u8) -> Result<GpioSetOutcome, BusError>;
    /// Drive `value` for `duration`, then restore the previous output value.
    fn pulse(
        &self,
        chip: &str,
        line: u32,
        value: u8,
        duration: std::time::Duration,
    ) -> Result<GpioPulseOutcome, BusError>;
}

#[cfg(target_os = "linux")]
type OutputHandle = std::sync::Arc<std::sync::Mutex<gpio_cdev::LineHandle>>;

/// Real Linux GPIO character-device backend using `gpio-cdev`.
///
/// Output lines that the node has driven are kept requested for the life of
/// the process. Dropping a gpio-cdev output handle releases the line (usually
/// back to input), so caching is what makes `set` a latched output and lets a
/// later `pulse` restore the value that was actually driven.
#[derive(Default)]
pub struct RealGpioBackend {
    #[cfg(target_os = "linux")]
    outputs: std::sync::Mutex<BTreeMap<(String, u32), OutputHandle>>,
}

impl GpioBackend for RealGpioBackend {
    fn info(&self, chip: &str, line: u32) -> Result<GpioLineInfo, BusError> {
        #[cfg(target_os = "linux")]
        {
            info_real(chip, line)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (chip, line);
            Err(unsupported())
        }
    }

    fn get(&self, chip: &str, line: u32) -> Result<u8, BusError> {
        #[cfg(target_os = "linux")]
        {
            get_real(self, chip, line)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (chip, line);
            Err(unsupported())
        }
    }

    fn set(&self, chip: &str, line: u32, value: u8) -> Result<GpioSetOutcome, BusError> {
        #[cfg(target_os = "linux")]
        {
            set_real(self, chip, line, value)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (chip, line, value);
            Err(unsupported())
        }
    }

    fn pulse(
        &self,
        chip: &str,
        line: u32,
        value: u8,
        duration: std::time::Duration,
    ) -> Result<GpioPulseOutcome, BusError> {
        #[cfg(target_os = "linux")]
        {
            pulse_real(self, chip, line, value, duration)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (chip, line, value, duration);
            Err(unsupported())
        }
    }
}

#[allow(dead_code)]
fn unsupported() -> BusError {
    BusError::Unsupported {
        detail: "GPIO is only implemented on Linux GPIO character devices".to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn info_real(chip: &str, line: u32) -> Result<GpioLineInfo, BusError> {
    let (_, _, info) = open_line(chip, line)?;
    let kernel_direction = match info.direction() {
        gpio_cdev::LineDirection::In => GpioDirection::Input,
        gpio_cdev::LineDirection::Out => GpioDirection::Output,
    };
    Ok(GpioLineInfo {
        chip: chip.to_owned(),
        line,
        device: device_path(chip),
        available: true,
        kernel_direction: Some(kernel_direction),
        kernel_name: info.name().map(str::to_owned),
        consumer: info.consumer().map(str::to_owned),
        kernel: info.is_kernel(),
        active_low: info.is_active_low(),
        open_drain: info.is_open_drain(),
        open_source: info.is_open_source(),
        bias: None,
    })
}

#[cfg(target_os = "linux")]
fn cached_output(backend: &RealGpioBackend, chip: &str, line: u32) -> Option<OutputHandle> {
    backend
        .outputs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(chip.to_owned(), line))
        .cloned()
}

/// Return the cached output handle for an exact line, requesting it the first
/// time. `initial` is the value to drive when the handle is newly requested;
/// `None` samples the current line so a pulse can restore it.
#[cfg(target_os = "linux")]
fn ensure_output(
    backend: &RealGpioBackend,
    chip: &str,
    line: u32,
    initial: Option<u8>,
) -> Result<(OutputHandle, Option<u8>), BusError> {
    use gpio_cdev::LineRequestFlags;

    let mut outputs = backend
        .outputs
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = (chip.to_owned(), line);
    if let Some(handle) = outputs.get(&key) {
        return Ok((handle.clone(), None));
    }

    let (_, line_ref, info) = open_line(chip, line)?;
    let active_low = info.is_active_low();
    let (default, sampled) = match initial {
        Some(value) => (value, None),
        None => {
            let mut input_flags = LineRequestFlags::INPUT;
            if active_low {
                input_flags |= LineRequestFlags::ACTIVE_LOW;
            }
            // Sampling by input is only for an explicit write/pulse. GET never
            // calls this path for an output line.
            let input = line_ref
                .request(input_flags, 0, GPIO_CONSUMER)
                .map_err(|error| map_gpio_error(&format!("sampling {chip} line {line}"), error))?;
            let previous = input
                .get_value()
                .map_err(|error| map_gpio_error(&format!("reading {chip} line {line}"), error))?;
            drop(input);
            (previous, Some(previous))
        }
    };

    let mut output_flags = LineRequestFlags::OUTPUT;
    if active_low {
        output_flags |= LineRequestFlags::ACTIVE_LOW;
    }
    // The request's default is the sampled/requested value, so even a failure
    // before the explicit set leaves the line at a known value.
    let output = line_ref
        .request(output_flags, default, GPIO_CONSUMER)
        .map_err(|error| map_gpio_error(&format!("requesting {chip} line {line} output"), error))?;
    let handle = std::sync::Arc::new(std::sync::Mutex::new(output));
    outputs.insert(key, handle.clone());
    Ok((handle, sampled))
}

#[cfg(target_os = "linux")]
fn get_real(backend: &RealGpioBackend, chip: &str, line: u32) -> Result<u8, BusError> {
    use gpio_cdev::LineRequestFlags;
    if let Some(handle) = cached_output(backend, chip, line) {
        return handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_value()
            .map_err(|error| map_gpio_error(&format!("reading {chip} line {line}"), error));
    }
    let (_, line_ref, info) = open_line(chip, line)?;
    let mut flags = LineRequestFlags::INPUT;
    if info.is_active_low() {
        flags |= LineRequestFlags::ACTIVE_LOW;
    }
    let handle = line_ref
        .request(flags, 0, GPIO_CONSUMER)
        .map_err(|error| map_gpio_error(&format!("requesting {chip} line {line} input"), error))?;
    handle
        .get_value()
        .map_err(|error| map_gpio_error(&format!("reading {chip} line {line}"), error))
}

#[cfg(target_os = "linux")]
fn set_real(
    backend: &RealGpioBackend,
    chip: &str,
    line: u32,
    value: u8,
) -> Result<GpioSetOutcome, BusError> {
    let (handle, _) = ensure_output(backend, chip, line, Some(value))?;
    let guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .set_value(value)
        .map_err(|error| map_gpio_error(&format!("driving {chip} line {line}"), error))?;
    let observed = guard.get_value().unwrap_or(value);
    Ok(GpioSetOutcome { value: observed })
}

#[cfg(target_os = "linux")]
fn pulse_real(
    backend: &RealGpioBackend,
    chip: &str,
    line: u32,
    value: u8,
    duration: std::time::Duration,
) -> Result<GpioPulseOutcome, BusError> {
    let (handle, sampled) = ensure_output(backend, chip, line, None)?;
    let guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = match sampled {
        Some(previous) => previous,
        None => guard
            .get_value()
            .map_err(|error| map_gpio_error(&format!("reading {chip} line {line}"), error))?,
    };
    guard
        .set_value(value)
        .map_err(|error| map_gpio_error(&format!("driving {chip} line {line}"), error))?;
    std::thread::sleep(duration);
    let pulsed = guard.get_value().unwrap_or(value);
    guard.set_value(previous).map_err(|error| {
        map_gpio_error(
            &format!("restoring {chip} line {line} to {previous}"),
            error,
        )
    })?;
    let restored = guard.get_value().unwrap_or(previous);
    Ok(GpioPulseOutcome {
        previous,
        pulsed,
        restored,
    })
}

#[cfg(target_os = "linux")]
fn open_line(
    chip: &str,
    line: u32,
) -> Result<(gpio_cdev::Chip, gpio_cdev::Line, gpio_cdev::LineInfo), BusError> {
    let device = device_path(chip);
    if !std::path::Path::new(&device).exists() {
        return Err(unavailable(format!(
            "GPIO chip {chip} device {device} is not present"
        )));
    }
    let mut chip_handle = gpio_cdev::Chip::new(&device)
        .map_err(|error| map_gpio_error(&format!("opening {device}"), error))?;
    let lines = chip_handle.num_lines();
    if line >= lines {
        return Err(invalid_request(format!(
            "{chip} has {lines} lines; offset {line} is out of range"
        )));
    }
    let line_ref = chip_handle
        .get_line(line)
        .map_err(|error| map_gpio_error(&format!("selecting {chip} line {line}"), error))?;
    let info = line_ref
        .info()
        .map_err(|error| map_gpio_error(&format!("reading {chip} line {line} info"), error))?;
    Ok((chip_handle, line_ref, info))
}

#[cfg(target_os = "linux")]
fn map_gpio_error(context: &str, error: gpio_cdev::Error) -> BusError {
    let detail = bounded_detail(&format!("{context}: {error}"));
    let lower = detail.to_ascii_lowercase();
    if lower.contains("permission denied") || lower.contains("operation not permitted") {
        BusError::PermissionDenied { detail }
    } else if lower.contains("busy") {
        BusError::Busy { detail }
    } else if lower.contains("no such file") || lower.contains("not found") {
        BusError::Unavailable { detail }
    } else if lower.contains("offset") && lower.contains("out of range") {
        BusError::InvalidRequest { detail }
    } else {
        BusError::IoFailed { detail }
    }
}

fn bounded_detail(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

/// One line in `GET /buses/gpio`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GpioLineSummary {
    pub chip: String,
    pub line: u32,
    pub direction: GpioDirection,
    pub device: String,
    pub available: bool,
}

/// `GET /buses/gpio` response. It only reads configuration and stats chips.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GpioListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub lines: Vec<GpioLineSummary>,
    pub limits: BusLimits,
}

/// `GET /buses/gpio/{chip}/{line}` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GpioDetailResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub available: bool,
    pub chip: String,
    pub line: u32,
    pub device: String,
    /// Direction from the operator allowlist.
    pub direction: GpioDirection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_direction: Option<GpioDirection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumer: Option<String>,
    pub kernel: bool,
    pub active_low: bool,
    pub open_drain: bool,
    pub open_source: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bias: Option<String>,
    /// Value is only read for an idle input line; an output line is never
    /// re-requested as an input by a GET.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<u8>,
}

/// Optional bounded pulse on a set request.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GpioPulseRequest {
    /// Pulse width in milliseconds, 1–5000.
    pub duration_ms: u64,
}

/// `POST /buses/gpio/{chip}/{line}/set` body.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GpioSetRequest {
    /// 0 or 1.
    pub value: u8,
    /// When present, drive `value` for the bounded duration and restore the
    /// previous output value.
    #[serde(default)]
    pub pulse: Option<GpioPulseRequest>,
}

/// `POST /buses/gpio/{chip}/{line}/set` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GpioSetResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub chip: String,
    pub line: u32,
    pub direction: GpioDirection,
    pub value: u8,
    pub pulsed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_value: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_value: Option<u8>,
}

impl BusState {
    pub(super) fn gpio_list(&self) -> Result<GpioListResponse, BusError> {
        if !self.inner.config.gpio.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let limit = self.inner.config.max_devices;
        let mut lines = Vec::new();
        for chip in self.inner.config.gpio.chips() {
            let device = device_path(chip);
            let available = std::path::Path::new(&device).exists();
            for (line, direction) in self.inner.config.gpio.lines(chip).into_iter().flatten() {
                lines.push(GpioLineSummary {
                    chip: chip.clone(),
                    line: *line,
                    direction: *direction,
                    device: device.clone(),
                    available,
                });
            }
        }
        let matched = lines.len();
        let truncated = matched > limit;
        lines.truncate(limit);
        Ok(GpioListResponse {
            ok: true,
            supported: true,
            configured: true,
            count: lines.len(),
            limit,
            truncated,
            lines,
            limits: self.limits(),
        })
    }

    pub(super) fn gpio_detail(
        &self,
        chip_raw: &str,
        line_raw: &str,
    ) -> Result<GpioDetailResponse, BusError> {
        if !self.inner.config.gpio.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let chip = parse_chip(chip_raw)?;
        let line = parse_line(line_raw)?;
        let direction = self
            .inner
            .config
            .gpio
            .direction(&chip, line)
            .ok_or_else(|| BusError::NotAllowed {
                detail: format!(
                    "GPIO {chip} line {line} is not allowlisted; add --gpio-allow {chip}:{line}:input"
                ),
            })?;
        let info = self.inner.gpio.info(&chip, line)?;
        // Never turn an output line into an input just to satisfy a GET.
        let value = if info.kernel || info.kernel_direction == Some(GpioDirection::Output) {
            None
        } else {
            Some(self.inner.gpio.get(&chip, line)?)
        };
        Ok(GpioDetailResponse {
            ok: true,
            supported: true,
            configured: true,
            available: info.available,
            chip,
            line,
            device: info.device,
            direction,
            kernel_direction: info.kernel_direction,
            kernel_name: info.kernel_name,
            consumer: info.consumer,
            kernel: info.kernel,
            active_low: info.active_low,
            open_drain: info.open_drain,
            open_source: info.open_source,
            bias: info.bias,
            value,
        })
    }

    pub(super) fn gpio_set(
        &self,
        chip_raw: &str,
        line_raw: &str,
        request: GpioSetRequest,
    ) -> Result<GpioSetResponse, BusError> {
        if !self.inner.config.gpio.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let chip = parse_chip(chip_raw)?;
        let line = parse_line(line_raw)?;
        let direction = self.inner.config.gpio.direction(&chip, line).ok_or_else(|| {
            BusError::NotAllowed {
                detail: format!(
                    "GPIO {chip} line {line} is not allowlisted; add --gpio-allow {chip}:{line}:output"
                ),
            }
        })?;
        if direction != GpioDirection::Output {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "GPIO {chip} line {line} is allowlisted as input; change it to --gpio-allow {chip}:{line}:output to write"
                ),
            });
        }
        if request.value > 1 {
            return Err(invalid_request(format!(
                "GPIO value must be 0 or 1, got {}",
                request.value
            )));
        }
        let duration_ms = request.pulse.map(|pulse| pulse.duration_ms);
        if let Some(duration_ms) = duration_ms {
            if !(MIN_GPIO_PULSE_MS..=MAX_GPIO_PULSE_MS).contains(&duration_ms) {
                return Err(invalid_request(format!(
                    "GPIO pulse duration must be between {MIN_GPIO_PULSE_MS} and {MAX_GPIO_PULSE_MS} ms"
                )));
            }
        }
        let _guard = self.lock_gpio_line(&chip, line)?;
        match duration_ms {
            Some(duration_ms) => {
                let outcome = self.inner.gpio.pulse(
                    &chip,
                    line,
                    request.value,
                    std::time::Duration::from_millis(duration_ms),
                )?;
                Ok(GpioSetResponse {
                    ok: true,
                    supported: true,
                    configured: true,
                    chip,
                    line,
                    direction,
                    value: outcome.pulsed,
                    pulsed: true,
                    duration_ms: Some(duration_ms),
                    previous_value: Some(outcome.previous),
                    restored_value: Some(outcome.restored),
                })
            }
            None => {
                let outcome = self.inner.gpio.set(&chip, line, request.value)?;
                Ok(GpioSetResponse {
                    ok: true,
                    supported: true,
                    configured: true,
                    chip,
                    line,
                    direction,
                    value: outcome.value,
                    pulsed: false,
                    duration_ms: None,
                    previous_value: None,
                    restored_value: None,
                })
            }
        }
    }
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/gpio", get(list_handler))
        .route("/buses/gpio/:chip/:line", get(detail_handler))
        .route(
            "/buses/gpio/:chip/:line/set",
            post(set_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.gpio_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "GPIO list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn detail_handler(
    State(state): State<BusState>,
    AxumPath((chip, line)): AxumPath<(String, String)>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.gpio_detail(&chip, &line)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "GPIO detail task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn set_handler(
    State(state): State<BusState>,
    AxumPath((chip, line)): AxumPath<(String, String)>,
    body: Result<axum::Json<GpioSetRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.gpio_set(&chip, &line, request)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "GPIO set task failed: {}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::can::RealCanBackend;
    use crate::buses::i2c::RealI2cBackend;
    use crate::buses::spi::RealSpiBackend;
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Info(String, u32),
        Get(String, u32),
        Set(String, u32, u8),
        Pulse(String, u32, u8, u64),
    }

    struct FakeInner {
        calls: Vec<Call>,
        info: GpioLineInfo,
        values: VecDeque<Result<u8, BusError>>,
        set_results: VecDeque<Result<GpioSetOutcome, BusError>>,
        pulse_results: VecDeque<Result<GpioPulseOutcome, BusError>>,
    }

    #[derive(Clone)]
    struct FakeGpio {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl Default for FakeGpio {
        fn default() -> Self {
            Self {
                inner: Arc::new(Mutex::new(FakeInner {
                    calls: Vec::new(),
                    info: GpioLineInfo {
                        chip: "gpiochip7".to_owned(),
                        line: 3,
                        device: "/dev/gpiochip7".to_owned(),
                        available: true,
                        kernel_direction: Some(GpioDirection::Input),
                        kernel_name: Some("TEST".to_owned()),
                        consumer: None,
                        kernel: false,
                        active_low: false,
                        open_drain: false,
                        open_source: false,
                        bias: None,
                    },
                    values: VecDeque::new(),
                    set_results: VecDeque::new(),
                    pulse_results: VecDeque::new(),
                })),
            }
        }
    }

    impl FakeGpio {
        fn calls(&self) -> Vec<Call> {
            self.inner.lock().unwrap().calls.clone()
        }
    }

    impl GpioBackend for FakeGpio {
        fn info(&self, chip: &str, line: u32) -> Result<GpioLineInfo, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Info(chip.to_owned(), line));
            Ok(inner.info.clone())
        }

        fn get(&self, chip: &str, line: u32) -> Result<u8, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Get(chip.to_owned(), line));
            inner.values.pop_front().unwrap_or(Ok(1))
        }

        fn set(&self, chip: &str, line: u32, value: u8) -> Result<GpioSetOutcome, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Set(chip.to_owned(), line, value));
            inner
                .set_results
                .pop_front()
                .unwrap_or(Ok(GpioSetOutcome { value }))
        }

        fn pulse(
            &self,
            chip: &str,
            line: u32,
            value: u8,
            duration: std::time::Duration,
        ) -> Result<GpioPulseOutcome, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Pulse(
                chip.to_owned(),
                line,
                value,
                duration.as_millis() as u64,
            ));
            inner
                .pulse_results
                .pop_front()
                .unwrap_or(Ok(GpioPulseOutcome {
                    previous: 0,
                    pulsed: value,
                    restored: 0,
                }))
        }
    }

    fn config(entries: &[&str]) -> BusConfig {
        BusConfig::new()
            .with_gpio_allow(
                &entries
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("valid GPIO allowlist")
    }

    fn state(config: BusConfig, fake: FakeGpio) -> BusState {
        BusState::with_all_backends(
            config,
            Arc::new(RealI2cBackend),
            Arc::new(RealSpiBackend),
            Arc::new(fake),
            Arc::new(RealCanBackend::default()),
        )
        .expect("valid bus state")
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
    fn allowlist_is_exact_and_direction_defaults_to_input() {
        let config =
            GpioConfig::from_entries(["gpiochip0:3, /dev/gpiochip1:4:output", "0:3"]).unwrap();
        assert!(config.is_configured());
        assert!(config.allows("gpiochip0", 3));
        assert_eq!(config.direction("gpiochip0", 3), Some(GpioDirection::Input));
        assert!(config.allows("gpiochip1", 4));
        assert_eq!(
            config.direction("gpiochip1", 4),
            Some(GpioDirection::Output)
        );
        assert!(!config.allows("gpiochip0", 4));
        assert!(!config.allows("gpiochip2", 3));
        assert_eq!(config.total_lines(), 2);

        for bad in [
            "gpiochip0",
            "gpiochip0:",
            "gpiochip0:-1",
            "gpiochip0:3:sideways",
            "foo:3",
            "gpiochip0:3:output,input",
            "gpiochip0:3:output:extra",
            "gpiochip:3",
        ] {
            assert!(
                GpioConfig::from_entries([bad]).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(GpioConfig::from_entries(["gpiochip0:3, gpiochip0:3:output"]).is_err());
    }

    #[test]
    fn route_chip_and_line_segments_are_normalized() {
        for raw in ["gpiochip0", "/dev/gpiochip0", "0", "00"] {
            assert_eq!(parse_chip(raw).unwrap(), "gpiochip0", "{raw}");
        }
        for bad in ["", "gpiochip", "gpiochipX", "gpiochip0/1", "../gpiochip0"] {
            assert!(parse_chip(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_line("3").unwrap(), 3);
        assert_eq!(parse_line("0x10").unwrap(), 16);
        assert!(parse_line("-1").is_err());
        assert!(parse_line("line").is_err());
    }

    #[tokio::test]
    async fn list_only_stats_configured_lines() {
        let fake = FakeGpio::default();
        let app = router(state(
            config(&["gpiochip7:3", "gpiochip7:4:output"]),
            fake.clone(),
        ));
        let (status, body) = get(&app, "/buses/gpio").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], true);
        assert_eq!(body["count"], 2);
        assert_eq!(body["lines"][0]["chip"], "gpiochip7");
        assert_eq!(body["lines"][0]["line"], 3);
        assert_eq!(body["lines"][0]["direction"], "input");
        assert_eq!(body["lines"][1]["direction"], "output");
        assert!(fake.calls().is_empty(), "listing must not open chips");
    }

    #[tokio::test]
    async fn detail_reports_kernel_facts_and_reads_idle_inputs_only() {
        let fake = FakeGpio::default();
        fake.inner.lock().unwrap().values.push_back(Ok(1));
        let app = router(state(config(&["gpiochip7:3"]), fake.clone()));

        let (status, body) = get(&app, "/buses/gpio/gpiochip7/3").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["direction"], "input");
        assert_eq!(body["kernel_direction"], "input");
        assert_eq!(body["kernel_name"], "TEST");
        assert_eq!(body["active_low"], false);
        assert_eq!(body["value"], 1);
        assert_eq!(
            fake.calls(),
            vec![
                Call::Info("gpiochip7".to_owned(), 3),
                Call::Get("gpiochip7".to_owned(), 3)
            ]
        );

        let output_info = FakeGpio::default();
        output_info.inner.lock().unwrap().info.kernel_direction = Some(GpioDirection::Output);
        let app = router(state(config(&["gpiochip7:3"]), output_info.clone()));
        let (status, body) = get(&app, "/buses/gpio/gpiochip7/3").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], Value::Null);
        assert_eq!(
            output_info.calls(),
            vec![Call::Info("gpiochip7".to_owned(), 3)],
            "an output line must not be re-requested as input by GET"
        );
    }

    #[tokio::test]
    async fn set_and_pulse_require_an_exact_output_line() {
        let fake = FakeGpio::default();
        let app = router(state(
            config(&["gpiochip7:3:output", "gpiochip7:4:input"]),
            fake.clone(),
        ));

        let (status, body) = post(&app, "/buses/gpio/gpiochip7/3/set", json!({"value": 1})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["value"], 1);
        assert_eq!(body["pulsed"], false);

        let (status, body) = post(
            &app,
            "/buses/gpio/gpiochip7/3/set",
            json!({"value": 1, "pulse": {"duration_ms": 25}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["pulsed"], true);
        assert_eq!(body["duration_ms"], 25);
        assert_eq!(body["previous_value"], 0);
        assert_eq!(body["restored_value"], 0);
        assert_eq!(
            fake.calls(),
            vec![
                Call::Set("gpiochip7".to_owned(), 3, 1),
                Call::Pulse("gpiochip7".to_owned(), 3, 1, 25)
            ]
        );

        let before = fake.calls().len();
        for uri in [
            "/buses/gpio/gpiochip7/4/set",
            "/buses/gpio/gpiochip7/9/set",
            "/buses/gpio/gpiochip8/3/set",
        ] {
            let (status, response) = post(&app, uri, json!({"value": 1})).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
            assert_eq!(response["code"], "bus.not-allowed", "{uri}");
        }
        for body in [
            json!({"value": 2}),
            json!({"value": 1, "pulse": {"duration_ms": 0}}),
            json!({"value": 1, "pulse": {"duration_ms": 5001}}),
            json!({"value": 1, "extra": true}),
        ] {
            let (status, response) = post(&app, "/buses/gpio/gpiochip7/3/set", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(response["code"], "bus.invalid-request");
        }
        assert_eq!(fake.calls().len(), before, "rejections never reach backend");
    }

    #[tokio::test]
    async fn backend_errors_map_to_typed_codes() {
        let fake = FakeGpio::default();
        fake.inner
            .lock()
            .unwrap()
            .set_results
            .push_back(Err(BusError::Busy {
                detail: "line claimed".to_owned(),
            }));
        let app = router(state(config(&["gpiochip7:3:output"]), fake.clone()));
        let (status, body) = post(&app, "/buses/gpio/gpiochip7/3/set", json!({"value": 1})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "bus.busy");

        let fake = FakeGpio::default();
        fake.inner
            .lock()
            .unwrap()
            .set_results
            .push_back(Err(BusError::PermissionDenied {
                detail: "EACCES".to_owned(),
            }));
        let app = router(state(config(&["gpiochip7:3:output"]), fake));
        let (status, body) = post(&app, "/buses/gpio/gpiochip7/3/set", json!({"value": 1})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.permission-denied");
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed() {
        let fake = FakeGpio::default();
        let app = router(state(BusConfig::default(), fake.clone()));
        let (status, body) = get(&app, "/buses/gpio").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        let (status, body) = get(&app, "/buses/gpio/gpiochip7/3").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        let (status, body) = post(&app, "/buses/gpio/gpiochip7/3/set", json!({"value": 1})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        assert!(fake.calls().is_empty());
    }

    /// Live evidence against a Linux `gpio-sim` virtual chip.
    ///
    /// Ignored by default; create the simulator explicitly and never point this
    /// test at a physical chip/line. Typical invocation:
    ///
    /// ```text
    /// sudo modprobe gpio-sim
    /// sudo mkdir /sys/kernel/config/gpio-sim/ozma-test
    /// echo 4 | sudo tee /sys/kernel/config/gpio-sim/ozma-test/num_lines
    /// sudo mkdir /sys/kernel/config/gpio-sim/ozma-test/line0
    /// echo 1 | sudo tee /sys/kernel/config/gpio-sim/ozma-test/live
    /// # find the new /dev/gpiochipN and set OZMA_LIVE_GPIO_CHIP=gpiochipN
    /// sudo env OZMA_LIVE_GPIO_CHIP=gpiochipN OZMA_LIVE_GPIO_LINE=0 cargo test \
    ///   -p ozma-node --lib buses::gpio::tests::live_gpio_sim -- --ignored --nocapture
    /// sudo rmdir /sys/kernel/config/gpio-sim/ozma-test/line0
    /// sudo rmdir /sys/kernel/config/gpio-sim/ozma-test
    /// sudo modprobe -r gpio_sim
    /// ```
    #[test]
    #[ignore = "requires a live Linux gpio-sim chip; set OZMA_LIVE_GPIO_CHIP"]
    fn live_gpio_sim_set_pulse_read() {
        let chip = std::env::var("OZMA_LIVE_GPIO_CHIP")
            .expect("set OZMA_LIVE_GPIO_CHIP to a gpio-sim chip");
        let line = std::env::var("OZMA_LIVE_GPIO_LINE")
            .unwrap_or_else(|_| "0".to_owned())
            .parse::<u32>()
            .expect("numeric OZMA_LIVE_GPIO_LINE");
        let allow = format!("{chip}:{line}:output");
        let config = BusConfig::new()
            .with_gpio_allow(&[allow])
            .expect("valid allowlist");
        let backend = Arc::new(RealGpioBackend::default());
        let state = BusState::with_all_backends(
            config,
            Arc::new(RealI2cBackend),
            Arc::new(RealSpiBackend),
            backend.clone(),
            Arc::new(RealCanBackend::default()),
        )
        .expect("valid state");

        let detail = state.gpio_detail(&chip, &line.to_string()).expect("detail");
        assert_eq!(detail.direction, GpioDirection::Output);
        assert!(detail.available);

        let set = state
            .gpio_set(
                &chip,
                &line.to_string(),
                GpioSetRequest {
                    value: 1,
                    pulse: None,
                },
            )
            .expect("set");
        assert_eq!(set.value, 1);
        assert!(!set.pulsed);

        let pulse = state
            .gpio_set(
                &chip,
                &line.to_string(),
                GpioSetRequest {
                    value: 1,
                    pulse: Some(GpioPulseRequest { duration_ms: 20 }),
                },
            )
            .expect("pulse");
        assert!(pulse.pulsed);
        assert_eq!(pulse.value, 1);
        let previous = pulse.previous_value.expect("previous value");
        assert_eq!(previous, 1, "set must latch until the pulse changes it");
        assert_eq!(pulse.restored_value, Some(previous));

        // Read the latched output value through the same backend cache used by
        // the state, proving the pulse restored it.
        assert_eq!(backend.get(&chip, line).expect("read back"), previous);

        let listed = state.gpio_list().expect("list");
        assert_eq!(listed.count, 1);
        assert_eq!(listed.lines[0].chip, chip);
    }
}
